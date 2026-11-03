//! Physical entry authenticates cached source formals and caller recovery roles.
use super::*;
use crate::compile::{CodeId, Executable, LibRun, Nested, NestedCallee, NestedKind};
use crate::machine::SlotState;
use lash_kernel_doc::{Value, arena::ArenaHandle};
use lash_kernel_state::arena::{PreparedBatch, PreparedInstallation};
use lash_kernel_value::content_ids::LinkId;
use logical::{PublicationValue as PV, VirtualBody, VirtualEntity};

/// Source caches the immutable entry interval, lexical slots and formal names.
/// This view retains those descriptors across detached parameter preparation.
pub(crate) struct FrameEntry<'e> {
    exe: &'e Executable,
    code: CodeId,
    source: Arc<Source>,
    interval: Arc<CompiledInterval>,
    slots: Arc<OwnerSlots>,
}
pub(crate) struct FrameCaller<'e> {
    pub(crate) source: Arc<Source>,
    pub(crate) invocation: Invocation,
    pub(crate) entry: &'e Nested,
    pub(crate) callable: Option<Value>,
    pub(crate) save: bool,
}
impl FrameEntry<'_> {
    pub(crate) fn compile(exe: &Executable, code: CodeId) -> Result<FrameEntry<'_>, Refused> {
        let source = Arc::clone(&exe.code(code).source_room);
        let slots = source.owner_slots()?;
        let interval = source.compile_event(
            &Point {
                site: source.site.clone(),
                phase: Phase::Enter,
            },
            ExitKind::Normal,
        )?;
        Ok(FrameEntry {
            exe,
            code,
            source,
            interval,
            slots,
        })
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "one actual detached frame and its source entry"
    )]
    pub(crate) fn prepare<'a>(
        &self,
        source: &Arc<Source>,
        link: LinkId,
        frontier: &Frontier,
        invocation: &Invocation,
        ledger: &Ledger,
        mut installation: PreparedInstallation<'a>,
        heap: &PagedHeap,
        arguments: &[(Option<Root>, Value)],
        states: &[SlotState],
        caller: Option<&FrameCaller<'_>>,
        inputs: &[Root],
        outputs: &[(Root, PV)],
        entities: &[VirtualEntity],
        handles: &[ArenaHandle],
    ) -> Result<Option<(BoundaryTransaction<'a>, PreparedBatch)>, HeapFault> {
        let code = self.exe.code(self.code);
        let position = frontier.position();
        let Root::Frame { task, .. } = invocation.frame() else {
            unreachable!()
        };
        let first = if source.body.is_empty() {
            Point {
                site: source.site.clone(),
                phase: Phase::Fallthrough,
            }
        } else {
            source
                .first_event(&source.site.child(0))
                .map_err(|_| fault("entry body has no source head"))?
        };
        if frontier.proven
            || !Arc::ptr_eq(source, &self.source)
            || !Arc::ptr_eq(source, &frontier.source)
            || !Arc::ptr_eq(&code.source_room, source)
            || !self.exe.instances.contains_key(&link)
            || self.interval.defining_source() != &source.site
            || position.at
                != (Point {
                    site: source.site.clone(),
                    phase: Phase::Enter,
                })
            || position.unpaid.as_ref() != Some(&first)
            || position.occurrence.task != task
            || position.occurrence.invocation != invocation.id()
            || !position.occurrence.loops.is_empty()
            || position.occurrence.action.is_some()
            || frontier.link != link
            || ledger.prospective_invocation(task) != *invocation
            || arguments.len() > source.params.len()
            || code.params.len() != source.params.len()
            || states.len() != code.slots_by_at.len()
        {
            return Ok(None);
        }
        let mut expected = Vec::new();
        let mut entity_index = 0;
        let mut captures = Vec::new();
        // Source formal order authenticates padding and the cell allocation order.
        for (index, param) in code.params.iter().enumerate() {
            let Some(declared) = code.slots.get(param.slot as usize) else {
                return Ok(None);
            };
            if declared.name != source.params[index]
                || declared.declared != source.site
                || declared.shared != param.shared
                || code.slots_by_at.get(param.at as usize) != Some(&param.slot)
                || param.shared
                    != source
                        .shared
                        .get(&(source.site.clone(), declared.name.clone()))
                        .copied()
                        .unwrap_or(false)
            {
                return Ok(None);
            }
            let (input, value) = arguments
                .get(index)
                .copied()
                .unwrap_or((None, Value::Absent));
            if let Some(input) = input
                && !inputs.contains(&input)
            {
                return Ok(None);
            }
            let Some(owner) = self.slots.root(
                &Owner::Binding {
                    declared: source.site.clone(),
                    name: declared.name.clone(),
                },
                task,
                invocation.id(),
            ) else {
                return Ok(None);
            };
            let actual = states.get(param.at as usize);
            let claim = if param.shared {
                let Some(VirtualEntity {
                    entity,
                    body: VirtualBody::NativeVariable(body),
                }) = entities.get(entity_index)
                else {
                    return Ok(None);
                };
                let Some(SlotState::Cell(cell)) = actual else {
                    return Ok(None);
                };
                if !body.same(&value)
                    || !matches!(installation.body(*cell), Ok(doc::arena::Body::Cell(actual)) if actual.same(&value))
                    || installation.ledger_key(*cell).ok() != Some(*entity)
                {
                    return Ok(None);
                }
                entity_index += 1;
                PV::Page(doc::heap::PageValue::Ref(doc::heap::Identity::Object(
                    *entity,
                )))
            } else {
                if !matches!(actual, Some(SlotState::Value(actual)) if actual.same(&value)) {
                    return Ok(None);
                }
                input.map_or(PV::Native(value), PV::Existing)
            };
            expected.push((owner, claim));
        }
        if entity_index != entities.len() {
            return Ok(None);
        }
        for (at, state) in states.iter().enumerate() {
            if code.params.iter().any(|param| param.at as usize == at)
                || matches!(state, SlotState::Empty)
            {
                continue;
            }
            let SlotState::Cell(cell) = state else {
                return Ok(None);
            };
            let Some(index) = code.slots_by_at.get(at) else {
                return Ok(None);
            };
            let Some(binding) = source
                .bindings
                .iter()
                .find(|binding| binding.index == *index)
            else {
                return Ok(None);
            };
            let Some(declared) = code.slots.get(*index as usize) else {
                return Ok(None);
            };
            if binding.name != declared.name
                || binding.declared != declared.declared
                || binding.shared != declared.shared
                || !(code
                    .captures
                    .iter()
                    .any(|capture| capture.inner_at as usize == at)
                    || code.free.iter().any(|(_, local)| local.at as usize == at))
            {
                return Ok(None);
            }
            let Some(owner) = source.binding_root(*index, task, invocation.id()) else {
                return Ok(None);
            };
            let claim = if matches!(
                owner,
                Root::Value {
                    slot: logical::Slot::Capture(_),
                    ..
                }
            ) {
                PV::Capture(*cell)
            } else {
                PV::Native(Value::ObjectRef(*cell))
            };
            expected.push((owner, claim));
        }
        for at in code
            .captures
            .iter()
            .map(|capture| capture.inner_at)
            .chain(code.free.iter().map(|(_, local)| local.at))
        {
            let Some(SlotState::Cell(cell)) = states.get(at as usize) else {
                return Ok(None);
            };
            captures.push(*cell);
        }
        let (mut registration, batch) = installation.into_parts();
        if let Some(caller) = caller {
            if !caller.check(
                self.exe,
                self.code,
                link,
                ledger,
                registration.arena(),
                heap,
                arguments,
                &captures,
                inputs,
                &mut expected,
            )? {
                return Ok(None);
            }
        } else if inputs.iter().any(|owner| {
            !arguments
                .iter()
                .any(|(input, _)| input.as_ref() == Some(owner))
        }) {
            return Ok(None);
        }
        if expected.len() != outputs.len()
            || expected
                .iter()
                .any(|(owner, _)| outputs.iter().filter(|(root, _)| root == owner).count() != 1)
        {
            return Ok(None);
        }
        let Some(dependencies) = Dependencies::capture(ledger, registration.arena(), handles)
        else {
            return Ok(None);
        };
        let mut actual = Vec::new();
        let mut claims = vec![(invocation.frame(), RootValue::Frame)];
        for (owner, value) in &expected {
            let (_, output) = outputs
                .iter()
                .find(|(root, _)| root == owner)
                .ok_or_else(|| fault("entry output disappeared"))?;
            let (Some(value), Some(output)) = (root_value(value), root_value(output)) else {
                return Ok(None);
            };
            claims.push((*owner, value));
            actual.push((*owner, output));
        }
        let mut transaction = ledger
            .prepare_frame_boundary(registration, heap, invocation, inputs, &actual, entities)?
            .bind_dependencies(dependencies)?;
        if !transaction.matches_roots(ledger, inputs, &claims) {
            return Ok(None);
        }
        Ok(Some((transaction, batch)))
    }
}
impl FrameCaller<'_> {
    #[expect(
        clippy::too_many_arguments,
        reason = "one source caller and actual callee"
    )]
    fn check(
        &self,
        exe: &Executable,
        target: CodeId,
        link: LinkId,
        ledger: &Ledger,
        arena: &Arena,
        heap: &PagedHeap,
        arguments: &[(Option<Root>, Value)],
        captures: &[ArenaHandle],
        inputs: &[Root],
        outputs: &mut Vec<(Root, PV)>,
    ) -> Result<bool, HeapFault> {
        let Root::Frame { task, .. } = self.invocation.frame() else {
            unreachable!()
        };
        if ledger
            .invocation(task, self.invocation.id(), heap.pages())
            .as_ref()
            .ok()
            != Some(&self.invocation)
        {
            return Ok(false);
        }
        let selected = match self.entry.kind {
            NestedKind::Call(NestedCallee::Declared(code)) => Some(code),
            NestedKind::Body(lib)
            | NestedKind::Call(NestedCallee::Library(lib) | NestedCallee::CheckedHelper(lib)) => {
                match exe.lib(lib).run {
                    LibRun::Body(code)
                    | LibRun::Native {
                        body: Some(code), ..
                    } => Some(code),
                    _ => None,
                }
            }
            NestedKind::Call(NestedCallee::Read) => match self.callable {
                Some(Value::Closure(handle)) => match arena.body(handle) {
                    Ok(doc::arena::Body::Closure {
                        identity,
                        captures: cells,
                        ..
                    }) => {
                        if identity.callable.link != link || cells.as_ref() != captures {
                            return Ok(false);
                        }
                        crate::machine::project::closure_code(
                            exe,
                            &identity.callable,
                            &identity.site.site,
                            &identity.capture_names,
                        )
                        .ok()
                    }
                    _ => None,
                },
                Some(Value::Function(handle)) => match arena.body(handle) {
                    Ok(doc::arena::Body::Function(callable))
                        if callable.link == link && exe.admits_callable(callable) =>
                    {
                        exe.declared.get(&callable.function).copied().or_else(|| {
                            let lib = exe.lib_of(&callable.function)?;
                            match exe.lib(lib).run {
                                LibRun::Body(code)
                                | LibRun::Native {
                                    body: Some(code), ..
                                } => Some(code),
                                _ => None,
                            }
                        })
                    }
                    _ => None,
                },
                Some(_) => None,
                None => None,
            },
            _ => None,
        };
        if selected != Some(target) {
            return Ok(false);
        }
        let slots = self
            .source
            .owner_slots()
            .map_err(|_| fault("caller owners failed verification"))?;
        let root = |selector: &Owner| slots.root(selector, task, self.invocation.id());
        let mut expected_inputs = Vec::new();
        let (phase, exit) = if self.entry.guest {
            (Phase::Enter, ExitKind::Action)
        } else {
            (Phase::Commit, ExitKind::Normal)
        };
        let Ok(entry) = self.source.compile_event(
            &Point {
                site: self.entry.site.clone(),
                phase,
            },
            exit,
        ) else {
            return Ok(false);
        };
        let event = entry
            .events()
            .next()
            .ok_or_else(|| fault("caller entry disappeared"))?;
        let mut roles = event.inputs.iter();
        if matches!(self.entry.kind, NestedKind::Call(NestedCallee::Read)) {
            let Some(input) = roles.next().and_then(root) else {
                return Ok(false);
            };
            expected_inputs.push(input);
        }
        let expected: Vec<_> = roles.map(root).collect();
        if expected.len() > arguments.len()
            || expected
                .iter()
                .zip(arguments)
                .any(|(expected, (actual, _))| expected != actual)
            || arguments[expected.len()..]
                .iter()
                .any(|(owner, value)| owner.is_some() || !matches!(value, Value::Absent))
        {
            return Ok(false);
        }
        expected_inputs.extend(arguments.iter().filter_map(|(owner, _)| *owner));
        let Ok(restore) = self.source.compile_event(
            &Point {
                site: self.entry.site.clone(),
                phase: Phase::Restore,
            },
            ExitKind::Normal,
        ) else {
            return Ok(false);
        };
        let Some(roles) = restore.restored_roles() else {
            return Ok(false);
        };
        if !roles
            .iter()
            .eq(self.entry.roles.iter().map(|role| &role.site))
        {
            return Ok(false);
        }
        for role in roles {
            let Some(output) = slots.continuation(role, task, self.invocation.id()) else {
                return Ok(false);
            };
            if self.save {
                let Some(input) = root(&Owner::Operand(role.clone())) else {
                    return Ok(false);
                };
                expected_inputs.push(input);
                outputs.push((output, PV::Existing(input)));
            } else if ledger.holding(output, heap.pages())?.is_none() {
                return Ok(false);
            }
        }
        Ok(inputs.len() == expected_inputs.len()
            && inputs.iter().all(|input| expected_inputs.contains(input))
            && inputs
                .iter()
                .enumerate()
                .all(|(index, input)| !inputs[..index].contains(input)))
    }
}

fn root_value(value: &PV) -> Option<RootValue<'_>> {
    Some(match value {
        PV::Existing(input) => RootValue::Existing(*input),
        PV::Native(value) => RootValue::Value(value),
        PV::Capture(cell) => RootValue::Capture(*cell),
        PV::Page(value) => RootValue::Page(value),
        _ => return None,
    })
}
