//! Precise constructors publish the body and its real operand-bank transition.
pub(in crate::machine) mod storage;
use super::*;
use crate::compile::room::{ExitKind, Phase, Point, publication::Occurrence};
use crate::machine::{
    Control, Halt, KernelMachine, fault, registers::PreparedConstruction, stream::Running,
};
use lash_kernel_exec::Op;
use lash_kernel_state::kernel_internals::logical::{Holding, JournalEvent, PublicationValue as PV};
use lash_kernel_value::tokens::{TokenCodec, TokenWriter, logical_digest};

struct SourceConstruction<'heap, 'bank> {
    recipe: storage::Construction<'heap>,
    bank: PreparedConstruction<'bank>,
    physical_room: u64,
}
struct PreparedSourceConstruction<'bank> {
    data: storage::Prepared,
    bank: PreparedConstruction<'bank>,
    holding: Holding,
}
impl<'bank> StoragePreparation for SourceConstruction<'_, 'bank> {
    type Prepared = PreparedSourceConstruction<'bank>;
    const MATERIALIZES: bool = true;
    fn prepare<'a>(
        self,
        arena: &'a mut Arena,
        actual: Program,
    ) -> Result<Option<PreparedStorage<'a, Self::Prepared>>, HeapFault> {
        // Compatibility eligibility only, until all paths use logical admission.
        // Physical storage estimates never supply a logical price or refusal.
        if arena.object_bound(&self.recipe.object) > self.physical_room {
            return Ok(None);
        }
        let heap = self.recipe.heap;
        let Some(PreparedStorage {
            registration,
            storage,
            actual,
        }) = self.recipe.prepare(arena, actual)?
        else {
            return Ok(None);
        };
        let [event] = actual.events() else {
            return Err(malformed("source construction lost its prepared event"));
        };
        let [(_, PV::Page(page))] = event.outputs.as_slice() else {
            return Err(malformed("source construction lost its canonical output"));
        };
        let holding = Holding::from_page(heap.pages(), page)?;
        Ok(Some(PreparedStorage {
            registration,
            storage: PreparedSourceConstruction {
                data: storage,
                bank: self.bank,
                holding,
            },
            actual,
        }))
    }
    fn facts(prepared: &Self::Prepared, facts: &mut LeafExecution) -> Result<(), HeapFault> {
        <storage::Construction<'_> as StoragePreparation>::facts(&prepared.data, facts)
    }
}

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
        #[cfg(any(test, feature = "testing"))]
        let output_register = op
            .writes()
            .ok_or_else(|| malformed("source construction has no output register"))?
            .ix();
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
        let (source, link) = {
            let frame = self.frame(running.task)?;
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
            (
                Arc::clone(&running.exe.code(frame.code).source_room),
                frame.link,
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
        let Some(bank) = self.regs.prepare_source_construction(running.stream, pc)? else {
            return Ok(false);
        };
        let output = bank.owner();
        let inputs = bank.owners();
        let values = bank.values();
        let dependencies = values.iter().filter_map(doc::Value::handle).collect();
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
        let recipe = storage::Construction {
            object,
            output,
            at: at.clone(),
            release,
            ledger: self.logical.ledger(),
            heap: &self.heap,
            depth_limit: self.limits.value_depth(),
        };
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
            || {
                Program::new(vec![JournalEvent {
                    inputs: inputs.clone(),
                    ..Default::default()
                }])
            },
            SourceConstruction {
                recipe,
                bank,
                physical_room,
            },
            |arena, _, edits, prepared| {
                debug_assert!(edits.is_empty());
                prepared.data.batch.publish(arena);
                prepared.bank.publish(prepared.data.value, prepared.holding);
            },
        )?;
        drop(arena);
        if published {
            self.grown();
            #[cfg(any(test, feature = "testing"))]
            if matches!(op, Op::MakeTuple { items } if items.len == 0) {
                crate::machine::islands::witness::empty_tuple_constructed(true);
            } else {
                crate::machine::islands::witness::precise_constructed(self.regs[output_register]);
            }
        }
        Ok(published)
    }
}
