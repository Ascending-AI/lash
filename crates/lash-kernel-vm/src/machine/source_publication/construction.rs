//! Precise constructors publish the body and its real operand-bank transition.
pub(in crate::machine) mod boundary;
pub(in crate::machine) mod storage;
use super::*;
use crate::compile::room::{ExitKind, Phase, Point, publication::Occurrence};
use crate::machine::{Halt, KernelMachine, fault, stream::Running};
use lash_kernel_exec::Op;
use lash_kernel_value::tokens::{TokenCodec, TokenWriter, logical_digest};

impl KernelMachine {
    pub(in crate::machine) fn publish_source_construction(
        &mut self,
        running: &Running<'_>,
        pc: u32,
    ) -> Result<bool, Halt> {
        let op = running.stream.ops[pc as usize];
        if !matches!(
            op,
            Op::MakeList { .. }
                | Op::MakeTuple { .. }
                | Op::MakeMap { .. }
                | Op::MakeSet { .. }
                | Op::MakeRecord { .. }
        ) {
            return Ok(false);
        }

        let names = if let Op::MakeRecord { fields, .. } = op {
            let Some(names) = running.stream.field_lists[fields.ix()]
                .iter()
                .map(|name| doc::arena::ShortText::new(name).map(doc::Value::Text))
                .collect::<Option<Vec<_>>>()
            else {
                return Ok(false);
            };
            Some(names)
        } else {
            None
        };
        let Some(Some(site)) = running.stream.source_sites.get(pc as usize) else {
            return Ok(false);
        };
        let physical_room = self.capacity_headroom();
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
        self.publish_logical_meter()?;
        let Some(bank) = self.regs.prepare_source_output(running.stream, pc)? else {
            return Ok(false);
        };
        let output = bank.owner();
        let inputs = bank.owners();
        let values = bank.values();
        let dependencies = values.iter().filter_map(doc::Value::handle).collect();
        let operands = values.clone();
        let object = match op {
            Op::MakeList { .. } => doc::NewObject::List(values),
            Op::MakeTuple { .. } => doc::NewObject::Tuple(values),
            Op::MakeSet { .. } => doc::NewObject::Set(values),
            Op::MakeMap { .. } => doc::NewObject::Map(
                values
                    .as_chunks::<2>()
                    .0
                    .iter()
                    .map(|[key, value]| (*key, *value))
                    .collect(),
            ),
            Op::MakeRecord { .. } => doc::NewObject::Record(
                names
                    .ok_or_else(|| malformed("source record has no field metadata"))?
                    .into_iter()
                    .zip(values)
                    .collect(),
            ),
            _ => unreachable!(),
        };
        if self.arena.borrow().object_bound(&object) > physical_room {
            return Ok(false);
        }
        let document = running
            .exe
            .instances
            .get(&link)
            .ok_or_else(|| malformed("source link has no admitted document"))?;
        let kernel = doc::KernelVersion::of(document.kernel())
            .ok_or_else(|| malformed("source kernel is unsupported"))?;
        let release = interval
            .events()
            .next()
            .ok_or_else(|| malformed("source construction has no event"))?
            .inputs
            .clone();
        let mut encoding = TokenWriter::new();
        encoding.seq(3);
        encoding.text("lash.source.precise-construction");
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
            .ok_or_else(|| malformed("source construction has no entered invocation"))?;
        let start = Position {
            at: at.clone(),
            unpaid: Some(at.clone()),
            recovery: logical_digest(&encoding.into_bytes()),
            occurrence: Occurrence {
                task: running.task,
                invocation: roots.invocation().id(),
                loops,
                action: None,
            },
        };
        let next = Position {
            at: at.clone(),
            unpaid: Some(unpaid),
            ..start.clone()
        };
        let safe_point = self.charged;
        let recipe = storage::Construction {
            charges: None,
            value: storage::ConstructionValue::Collection(object),
            output,
            at: at.clone(),
            release,
            inputs,
            operands,
            heap: &self.heap,
            depth_limit: self.limits.value_depth(),
        };
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
        .publish_external(
            roots.invocation(),
            &mut frame.source_frontier,
            Prefix {
                interval,
                facts: LeafExecution(
                    [(
                        at,
                        LeafFacts {
                            dependencies,
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
            recipe,
            |arena, _, edits, prepared| {
                debug_assert!(edits.is_empty());
                prepared.batch.publish(arena);
                bank.publish(prepared.value, prepared.holding);
            },
        )?;
        drop(arena);

        Ok(published)
    }
}
