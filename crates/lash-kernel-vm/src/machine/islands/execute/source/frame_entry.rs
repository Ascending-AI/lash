//! A complete packet callee and caller continuation install in one journal.
use super::*;
use crate::admission::{Allowance, SourceResources};
use crate::compile::{NestedCallee, NestedKind};
use lash_kernel_state::kernel_internals::logical::{
    FrameEvent, JournalEvent, Program, PublicationValue as PV,
};
use lash_kernel_value::tokens::{TokenCodec, TokenWriter, logical_digest};
use room::{
    ExitKind, Phase, Point,
    publication::{Occurrence, Position as SourcePosition},
};
use std::sync::Arc;

impl<'e, const OBSERVING: bool> Island<'e, OBSERVING> {
    pub(in crate::machine::islands::execute) fn publish_call_frame(
        &mut self,
        active: &mut Frame<'e>,
        target: Callee<'e>,
        continuation: CallContinuation<'e>,
        boundary: Option<crate::compile::LibId>,
        slab: &mut Slab,
        callers: &mut Vec<Frame<'e>>,
    ) -> Result<bool, HeapFault> {
        let site = continuation.site;
        if site.root.is_some() || self.projection().is_some() {
            return Ok(false);
        }
        let Some(act) = site.act else {
            return Ok(false);
        };
        let Some(id) = active.statement else {
            return Ok(false);
        };
        let (stream, statement, _) = self.exe.statement(id);
        let entry = stream
            .nested
            .get(act.ix())
            .ok_or_else(|| malformed("packet entry has no nested source"))?;
        let counted = entry.guest && matches!(entry.kind, NestedKind::Call(_));
        let body = matches!(entry.kind, NestedKind::Body(_));
        if !(counted || body) {
            return Ok(false);
        }
        let caller_source = &self.exe.code(active.callee.plan.code).source_room;
        let Some(caller) = active.source.as_ref() else {
            return Ok(false);
        };
        if counted {
            let Some(ordinal) = continuation.occurrence else {
                return Ok(false);
            };
            let Some(frontier) = &caller.frontier else {
                return Ok(false);
            };
            let at = &frontier.position().at;
            if at.site != entry.site || at.phase != Phase::Enter {
                return Ok(false);
            }
            if frontier.position().occurrence.action.as_ref() != Some(&(at.clone(), ordinal))
                || self
                    .counters
                    .current(entry.occurrence)
                    .is_none_or(|next| ordinal >= next)
            {
                return Err(malformed(
                    "packet entry lost its accepted caller occurrence",
                ));
            }
        } else if continuation.occurrence.is_some() {
            return Err(malformed(
                "uncounted packet entry acquired an action occurrence",
            ));
        }
        let Ok(restore) = caller_source.compile_event(
            &Point {
                site: entry.site.clone(),
                phase: Phase::Restore,
            },
            ExitKind::Normal,
        ) else {
            return Ok(false);
        };
        let Some(expected) = restore.restored_roles() else {
            return Ok(false);
        };
        if !expected
            .iter()
            .eq(entry.roles.iter().map(|role| &role.site))
            || !entry.site.path.starts_with(&statement.site.path)
            || entry.suffix.as_ref() != &entry.site.path[statement.site.path.len()..]
            || entry.roles.iter().any(|role| {
                role.site.unit != statement.site.unit
                    || !role.site.path.starts_with(&statement.site.path)
                    || role.suffix.as_ref() != &role.site.path[statement.site.path.len()..]
            })
        {
            return Err(malformed(
                "packet entry continuation differs from its source",
            ));
        }
        let Some(roles) = statement.source_roles_at(stream, site.pc) else {
            return Ok(false);
        };
        let slots = caller_source
            .owner_slots()
            .map_err(|_| malformed("packet caller owners failed verification"))?;
        let caller_invocation = caller.invocation.id();
        let input_owner = |reg: u32| -> Result<Option<Owner>, HeapFault> {
            let Some((_, at)) = roles.iter().find(|(input, _)| input.0 == reg) else {
                return Ok(None);
            };
            slots
                .root(
                    &room::Owner::Operand((**at).clone()),
                    self.task,
                    caller_invocation,
                )
                .map(Some)
                .ok_or_else(|| malformed("packet entry input has no source owner"))
        };
        self.logical.flush(self.arena, self.heap)?;
        let store = self.logical.ledger();
        let ledger = store.borrow();
        let invocation = ledger.prospective_invocation(self.task);
        let code = self.exe.code(target.plan.code);
        let source = Arc::clone(&code.source_room);
        source
            .verify()
            .map_err(|_| malformed("packet entry source failed verification"))?;
        if continuation.args.len() > code.params.len() || source.params.len() != code.params.len() {
            return Err(malformed("packet entry arity differs from its source"));
        }
        let mut arguments = Vec::new();
        let mut inputs = Vec::new();
        let mut outputs = Vec::new();
        let mut dependencies = Vec::new();
        let values = slab.view(active);
        for reg in continuation.args {
            let Some(owner) = input_owner(*reg)? else {
                return Ok(false);
            };
            if ledger.holding(owner, self.heap.pages())?.is_none() {
                return Ok(false);
            }
            inputs.push(owner);
            let value = values.regs[*reg as usize];
            arguments.push((Some(owner), value));
            dependencies.extend(value.handle());
        }
        if matches!(entry.kind, NestedKind::Call(NestedCallee::Read)) {
            let Some(owner) = input_owner(site.reg)? else {
                return Ok(false);
            };
            if ledger.holding(owner, self.heap.pages())?.is_none() {
                return Ok(false);
            }
            inputs.push(owner);
            dependencies.extend(values.regs[site.reg as usize].handle());
        }
        for role in &entry.roles {
            let Some((_, at)) = roles.iter().find(|(reg, _)| *reg == role.reg) else {
                return Ok(false);
            };
            if at.as_ref() != &role.site {
                return Err(malformed("packet entry saved another source operand"));
            }
            let Some(input) = input_owner(role.reg.0)? else {
                return Ok(false);
            };
            let output = slots
                .continuation(&role.site, self.task, caller_invocation)
                .ok_or_else(|| malformed("packet entry role has no continuation owner"))?;
            if ledger.holding(input, self.heap.pages())?.is_none() {
                return Ok(false);
            }
            if ledger.holding(output, self.heap.pages())?.is_some() {
                return Err(malformed("packet entry would replace a live continuation"));
            }
            inputs.push(input);
            outputs.push((output, PV::Existing(input)));
            dependencies.extend(values.regs[role.reg.ix()].handle());
        }
        slab.reserve_call(target)?;
        callers.try_reserve(1).map_err(|_| HeapFault::Exhausted {
            resource: doc::heap::HostResource::StagingBytes,
        })?;
        let window = self.scratch.windows.pop().unwrap_or_default();
        let start = target.captured as usize;
        let captured = self
            .scratch
            .captured
            .get(start..start + target.plan.captures.len())
            .ok_or_else(|| malformed("packet entry has no complete capture window"))?;
        dependencies.extend(captured.iter().map(|(_, cell)| *cell));
        let parameters::Parameters {
            installation,
            bindings,
            outputs: parameter_outputs,
            entities,
            shared,
        } = (parameters::EntryParameters {
            target,
            exe: self.exe,
            task: self.task,
            invocation: invocation.id(),
        })
        .prepare(self.arena, &arguments, captured)?;
        outputs.extend(parameter_outputs);
        let mut encoding = TokenWriter::new();
        encoding.seq(3);
        encoding.text("lash.source.packet-frame-entry");
        source.site.encode(&mut encoding);
        target.link.encode(&mut encoding);
        let unpaid = if source.body.is_empty() {
            Point {
                site: source.site.clone(),
                phase: Phase::Fallthrough,
            }
        } else {
            source
                .first_event(&source.site.child(0))
                .map_err(|_| malformed("packet callee has no first source event"))?
        };
        let document = self
            .exe
            .instances
            .get(&target.link)
            .ok_or_else(|| malformed("packet entry has no admitted defining link"))?;
        let kernel = doc::KernelVersion::of(document.kernel())
            .ok_or_else(|| malformed("packet entry kernel is unsupported"))?;
        let frontier = source
            .frontier(
                &ledger,
                self.heap,
                &invocation,
                target.link,
                kernel,
                SourcePosition {
                    at: Point {
                        site: source.site.clone(),
                        phase: Phase::Enter,
                    },
                    unpaid: Some(unpaid),
                    recovery: logical_digest(&encoding.into_bytes()),
                    occurrence: Occurrence {
                        task: self.task,
                        invocation: invocation.id(),
                        loops: vec![],
                        action: None,
                        departure: None,
                    },
                },
            )
            .map_err(|_| malformed("packet entry frontier failed authentication"))?;
        let program = Program::new(vec![JournalEvent {
            inputs,
            outputs,
            entities,
            frame: Some(FrameEvent::Enter {
                task: self.task,
                invocation: invocation.id(),
            }),
            ..Default::default()
        }]);
        let (mut registration, batch) = installation.into_parts();
        let evaluated = program
            .clone()
            .evaluate(&ledger, registration.arena())
            .and_then(|evaluated| {
                evaluated.bind_dependencies(&ledger, registration.arena(), &dependencies)
            })
            .ok_or_else(|| malformed("packet entry dependencies are unavailable"))?;
        let prepared = evaluated
            .prepare(&ledger, registration, self.heap, program)
            .map_err(unit::member_fault)?;
        let after = prepared.balance()?.total()?;
        let safe_point = self.charged;
        let allowance = Allowance {
            occurrences: vec![],
            before: 0,
            units: 0,
            close: None,
            strict: false,
            depth: 0,
            safe_point,
        };
        let resources = SourceResources {
            charged: &mut self.charged,
            counters: self.counters,
            bound: self.bound,
            slice: self.slice_end,
            depth: self.base + u64::from(self.depth),
            limit: self.call_depth,
            safe_point,
        };
        let facts = resources
            .facts(&allowance)
            .ok_or_else(|| malformed("packet entry resource facts are unavailable"))?;
        let Some(accepted) = prepared.admit(self.memory_bound) else {
            return Ok(false);
        };
        let Some(accepted) = resources.admit_interval(&facts, &allowance, accepted)? else {
            return Ok(false);
        };
        drop(ledger);
        accepted.commit(|accepted| {
            accepted.publish_materialized(&mut store.borrow_mut(), |arena, edits, _| {
                assert!(edits.is_empty());
                batch.publish(arena);
                let ranges = slab.call(active, target, continuation.args, captured);
                let mut inner = Frame::new(target, ranges, window);
                slab.view(&mut inner).slots.copy_from_slice(&bindings);
                inner.source = Some(Activation {
                    invocation,
                    frontier: Some(frontier),
                    cleaning: None,
                });
                inner.boundary = boundary;
                callers.push(std::mem::replace(active, inner));
            })
        })?;
        self.logical_memory.set(after);
        self.register_bytes += target.compiled.register_bytes();
        self.depth += 1;
        self.activate(&mut slab.view(active));
        #[cfg(any(test, feature = "testing"))]
        crate::machine::islands::witness::packet_frame_entered(shared);
        Ok(true)
    }
}
