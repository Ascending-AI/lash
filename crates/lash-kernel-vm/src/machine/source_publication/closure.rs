//! A precise closure publishes its prepared body and captured cells atomically.
use super::*;
use crate::compile::{
    CodeId,
    room::{ExitKind, Measure, Phase, Point, publication::Occurrence},
};
use crate::machine::{Halt, KernelMachine, SlotState, fault, stream::Running};
use lash_kernel_exec::Op;
use lash_kernel_state::kernel_internals::logical::{VirtualBody, VirtualEntity};
use lash_kernel_value::tokens::{TokenCodec, TokenWriter, logical_digest};

impl KernelMachine {
    pub(in crate::machine) fn publish_source_closure(
        &mut self,
        running: &Running<'_>,
        pc: u32,
    ) -> Result<bool, Halt> {
        let Op::Closure { code, .. } = running.stream.ops[pc as usize] else {
            return Ok(false);
        };
        let target = running.exe.code(CodeId(code.0));
        let Some(Some(site)) = running.stream.source_sites.get(pc as usize) else {
            return Ok(false);
        };
        if target.created.as_ref() != Some(site) {
            return Ok(false);
        }
        let bound = self.arena.borrow().sequence_bound(target.captures.len());
        if bound > self.capacity_headroom() {
            return Ok(false);
        }
        let lash_kernel_value::sites::Unit::Library(function) = target.site.unit else {
            return Ok(false);
        };
        let (source, link, captures, loops) = {
            let frame = self.frame(running.task)?;
            let Some(loops) = source_loops(frame, running.exe)? else {
                return Ok(false);
            };
            let mut captures = Vec::new();
            captures
                .try_reserve(target.captures.len())
                .map_err(|_| HeapFault::Exhausted {
                    resource: doc::heap::HostResource::StagingBytes,
                })?;
            for capture in &target.captures {
                let SlotState::Cell(cell) = frame.slot(capture.outer_at) else {
                    return Ok(false);
                };
                captures.push(*cell);
            }
            (
                Arc::clone(&running.exe.code(frame.code).source_room),
                frame.link,
                captures,
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
        // Captures may still be cold after reopen. Validate the actual cell
        // heads through the machine's retry owner before reserving a closure.
        for capture in &captures {
            loop {
                let cold = match self.arena.borrow().cell(*capture) {
                    Ok(_) => None,
                    Err(doc::NativeError::Retry(doc::Retry::Cold { object, chunk })) => {
                        Some((object, chunk))
                    }
                    Err(error) => return Err(native_fault(error).into()),
                };
                let Some((object, chunk)) = cold else { break };
                self.warm(object, chunk)?;
            }
        }
        let mut entities = Vec::new();
        entities
            .try_reserve(captures.len())
            .map_err(|_| HeapFault::Exhausted {
                resource: doc::heap::HostResource::StagingBytes,
            })?;
        for handle in &captures {
            let Some(entity) = self
                .arena
                .borrow()
                .facts(*handle)
                .ok()
                .and_then(|facts| facts.ledger_entity())
            else {
                return Ok(false);
            };
            entities.push(entity);
        }
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
        encoding.seq(4);
        encoding.text("lash.source.precise-closure");
        at.site.encode(&mut encoding);
        output.encode(&mut encoding);
        entities.encode(&mut encoding);
        let identity = Arc::new(doc::arena::ClosureIdentity {
            capture_names: target.capture_names.clone(),
            callable: lash_kernel_value::names::CallableId { link, function },
            site: doc::linked_site::LinkedSite {
                link,
                site: (**site).clone(),
            },
        });
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
            },
        };
        let next = Position {
            at: at.clone(),
            unpaid: Some(unpaid),
            ..start.clone()
        };
        let safe_point = self.charged;
        let facts = LeafExecution(
            [(
                at.clone(),
                LeafFacts {
                    dependencies: captures.clone(),
                    values: [(
                        Measure::CaptureEdges(at.site.clone()),
                        captures.len() as i128,
                    )]
                    .into(),
                    ..Default::default()
                },
            )]
            .into(),
        );
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
                facts,
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
            super::construction::storage::Construction {
                value: super::construction::storage::ConstructionValue::Closure {
                    identity,
                    captures,
                    code: doc::arena::LocalCode {
                        executable: running.exe.local(),
                        code: code.0,
                    },
                },
                output,
                at,
                release: vec![].into_boxed_slice(),
                inputs: vec![],
                operands: vec![],
                depth_limit: self.limits.value_depth(),
                heap: &self.heap,
            },
            |arena, _, edits, storage| {
                debug_assert!(edits.is_empty());
                storage.batch.publish(arena);
                bank.publish(storage.value, storage.holding);
            },
        )?;
        drop(arena);
        if published {
            #[cfg(any(test, feature = "testing"))]
            crate::machine::islands::witness::closure_constructed();
        }
        Ok(published)
    }
}

/// Capture identities come from the actual cells, in capture order. The fresh
/// closure takes the next source allocation ordinal under the same registration.
fn prepare_closure_body(
    arena: &mut Arena,
    identity: Arc<doc::arena::ClosureIdentity>,
    captures: Vec<ArenaHandle>,
    code: doc::arena::LocalCode,
) -> Result<
    (
        ArenaHandle,
        lash_kernel_state::arena::PreparedInstallation<'_>,
        VirtualEntity,
    ),
    HeapFault,
> {
    if identity.capture_names.len() != captures.len() {
        return Err(malformed(
            "closure capture names disagree with actual cells",
        ));
    }
    let entity = arena.next_provisional();
    let entities = captures
        .iter()
        .map(|handle| {
            arena.cell(*handle).map_err(native_fault)?;
            arena
                .facts(*handle)
                .map_err(native_fault)?
                .ledger_entity()
                .ok_or_else(|| malformed("closure capture has no source identity"))
        })
        .collect::<Result<Vec<_>, _>>()?;
    let (handle, mut installation) = arena
        .prepare_closure(identity, captures, code)
        .map_err(native_fault)?;
    installation
        .bind_entity(handle, entity)
        .map_err(native_fault)?;
    Ok((
        handle,
        installation,
        VirtualEntity {
            entity,
            body: VirtualBody::Closure(entities),
        },
    ))
}

pub(in crate::machine) fn prepare_closure<'a>(
    arena: &'a mut Arena,
    heap: &PagedHeap,
    identity: Arc<doc::arena::ClosureIdentity>,
    captures: Vec<ArenaHandle>,
    code: doc::arena::LocalCode,
    output: lash_kernel_state::kernel_internals::logical::Owner,
) -> Result<super::construction::boundary::PreparedGraph<'a>, HeapFault> {
    let (handle, installation, entity) = prepare_closure_body(arena, identity, captures, code)?;
    super::construction::boundary::finish(
        installation,
        heap,
        &[(output, doc::Value::Closure(handle))],
        vec![entity],
    )
}
