//! Precise list writes install their reserved member and bank edits together.
use super::*;
use crate::compile::room::{ExitKind, Measure, Phase, Point, publication::Occurrence};
use crate::machine::{Halt, KernelMachine, fault, stream::Running};
use lash_kernel_exec::Op;
use lash_kernel_state::kernel_internals::logical::{JournalEvent, Member, MemberEdit};
use lash_kernel_value::tokens::{TokenCodec, TokenWriter, logical_digest};

enum PreparedListMember {
    Write(lash_kernel_state::arena::PreparedListWrite),
    Remove(lash_kernel_state::arena::PreparedListRemove),
}
impl PreparedListMember {
    fn publish(self, arena: &mut Arena, edit: MemberEdit) {
        match self {
            Self::Write(write) => write.publish(arena, edit),
            Self::Remove(remove) => remove.publish(arena, edit),
        }
    }
}
impl KernelMachine {
    pub(in crate::machine) fn publish_source_list_member(
        &mut self,
        running: &Running<'_>,
        pc: u32,
    ) -> Result<bool, Halt> {
        let (target, removing) = match running.stream.ops[pc as usize] {
            Op::SetIndex { target, .. } => (target, false),
            Op::RemoveIndex { target } => (target, true),
            _ => return Ok(false),
        };
        let doc::Value::List(handle) = self.regs[target.ix()] else {
            return Ok(false);
        };
        let Some(index) = crate::compile::position(&self.regs[target.ix() + 1]) else {
            return Ok(false);
        };
        let Ok(len) = self.arena.borrow().len(handle) else {
            return Ok(false);
        };
        // The original guest range error precedes any member journal.
        if index > len || (removing && index == len) {
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
        let Some(bank) = self.regs.prepare_source_inputs(running.stream, pc)? else {
            return Ok(false);
        };
        let inputs = bank.owners();
        let values = bank.values();
        let next = match values.as_slice() {
            [doc::Value::List(actual), _] if removing && *actual == handle => None,
            [doc::Value::List(actual), _, value] if !removing && *actual == handle => Some(*value),
            _ => return Err(malformed("list member has another source input shape").into()),
        };
        let edit = MemberEdit {
            handle,
            member: Member::List(index),
            next,
        };
        let store = self.logical.ledger();
        let slots = source
            .owner_slots()
            .map_err(|_| malformed("list store owners failed verification"))?;
        let invocation = self
            .tasks
            .get_mut(running.task.0 as usize)?
            .ok_or_else(|| fault("no task"))?
            .frames
            .last_mut()?
            .ok_or_else(|| fault("no frame"))?
            .invocation;
        let mut arena = self.arena.borrow_mut();
        let ledger = store.borrow();
        let member = ledger
            .prepare_member_publication(
                arena.registration().map_err(native_fault)?,
                self.heap.pages(),
                handle,
                edit.member,
                edit.next,
                &[],
                &[],
            )
            .map_err(|error| match error {
                lash_kernel_state::kernel_internals::logical::MemberFault::Heap(error) => error,
                lash_kernel_state::kernel_internals::logical::MemberFault::Native(error) => {
                    native_fault(error)
                }
            })?;
        let delta = i128::from(member.balance()?.total()?) - i128::from(ledger.balance()?.total()?);
        let released = member.release_cost(&inputs, self.heap.pages())?;
        drop(member);
        let event = interval
            .events()
            .next()
            .ok_or_else(|| malformed("list store event disappeared"))?;
        let mut measures = std::collections::BTreeMap::new();
        for selector in &event.inputs {
            let owner = slots
                .root(selector, running.task, invocation)
                .ok_or_else(|| malformed("list store operand has no canonical owner"))?;
            if !inputs.contains(&owner) {
                return Err(malformed("list store has another source operand").into());
            }
            let holding = ledger
                .holding(owner, self.heap.pages())?
                .ok_or_else(|| malformed("list store operand is absent"))?;
            measures.insert(
                Measure::Local(selector.clone()),
                i128::from(holding.bytes()),
            );
        }
        measures.insert(Measure::MemberDelta(at.site.clone()), delta);
        measures.insert(Measure::Release(event.inputs.clone()), i128::from(released));
        drop(ledger);
        let document = running
            .exe
            .instances
            .get(&link)
            .ok_or_else(|| malformed("source link has no admitted document"))?;
        let kernel = doc::KernelVersion::of(document.kernel())
            .ok_or_else(|| malformed("source kernel is unsupported"))?;
        let mut encoding = TokenWriter::new();
        encoding.seq(3);
        encoding.text("lash.source.precise-list-store");
        at.site.encode(&mut encoding);
        inputs.encode(&mut encoding);
        let task = self
            .tasks
            .get_mut(running.task.0 as usize)?
            .ok_or_else(|| fault("no task"))?;
        let depth = (task.frames.len()
            - usize::from(running.task == doc::TaskId::MAIN && self.session_cell))
            as u64;
        let frame = task.frames.last_mut()?.ok_or_else(|| fault("no frame"))?;
        let roots = frame
            .logical
            .as_ref()
            .ok_or_else(|| malformed("list store has no entered invocation"))?;
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
                        at,
                        LeafFacts {
                            dependencies: std::iter::once(handle)
                                .chain(values.iter().filter_map(doc::Value::handle))
                                .collect(),
                            values: measures,
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
                    member: Some(edit),
                    ..Default::default()
                }])
            },
            Reserve(|arena: &mut Arena| match edit.next {
                Some(value) => arena
                    .prepare_list_write(handle, index, value)
                    .map(|write| write.map(PreparedListMember::Write)),
                None => arena
                    .prepare_list_remove(handle, index)
                    .map(|remove| remove.map(PreparedListMember::Remove)),
            }),
            |arena, _, edits, write| {
                let [edit] = edits.as_slice() else {
                    unreachable!("a checked list store changed member arity")
                };
                write.publish(arena, *edit);
                bank.publish();
            },
        )?;
        drop(arena);
        if published {
            if !removing {
                self.grown();
            }
            #[cfg(any(test, feature = "testing"))]
            if removing {
                crate::machine::islands::witness::list_removal_published();
            } else {
                crate::machine::islands::witness::member_published();
            }
        }
        Ok(published)
    }
}
