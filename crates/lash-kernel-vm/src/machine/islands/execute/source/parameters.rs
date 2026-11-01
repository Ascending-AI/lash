//! Both packet entry forms prepare the same canonical parameters and cells.
use super::*;
use lash_kernel_state::{
    arena::PreparedInstallation,
    kernel_internals::logical::{PublicationValue as PV, VirtualBody, VirtualEntity},
};

pub(super) struct Parameters<'a> {
    pub(super) installation: PreparedInstallation<'a>,
    pub(super) bindings: Vec<SlotState>,
    pub(super) outputs: Vec<(Owner, PV)>,
    pub(super) entities: Vec<VirtualEntity>,
    pub(super) shared: bool,
}
pub(super) struct EntryParameters<'e> {
    pub(super) target: Callee<'e>,
    pub(super) exe: &'e crate::compile::Executable,
    pub(super) task: doc::TaskId,
    pub(super) invocation: u64,
}
impl EntryParameters<'_> {
    pub(super) fn prepare<'a>(
        self,
        arena: &'a mut Arena,
        arguments: &[(Option<Owner>, Value)],
        captured: &[(Value, doc::arena::ArenaHandle)],
    ) -> Result<Parameters<'a>, HeapFault> {
        let Self {
            target,
            exe,
            task,
            invocation,
        } = self;
        let code = exe.code(target.plan.code);
        let source = &code.source_room;
        source
            .verify()
            .map_err(|_| malformed("packet parameter source failed verification"))?;
        if arguments.len() > code.params.len() || source.params.len() != code.params.len() {
            return Err(malformed("packet parameters differ from source arity"));
        }
        let mut outputs = Vec::new();
        let mut bindings = vec![SlotState::Empty; target.plan.slots];
        target.compiled.layout.fill(
            &mut bindings,
            arguments.iter().map(|(_, value)| *value),
            captured,
        );
        let mut shared_values = Vec::new();
        for (index, param) in code.params.iter().enumerate() {
            let declared = code
                .slots
                .get(param.slot as usize)
                .ok_or_else(|| malformed("packet parameter slot is absent"))?;
            if declared.name != source.params[index]
                || declared.declared != source.site
                || declared.shared != param.shared
                || code.slots_by_at.get(param.at as usize) != Some(&param.slot)
                || param.shared
                    != source
                        .shared
                        .get(&(source.site.clone(), source.params[index].clone()))
                        .copied()
                        .unwrap_or(false)
            {
                return Err(malformed(
                    "packet parameter differs from its source declaration",
                ));
            }
            if param.shared {
                shared_values.push(
                    arguments
                        .get(index)
                        .map_or(Value::Absent, |(_, value)| *value),
                );
            }
        }
        let first = arena.next_provisional().ordinal;
        let (handles, mut installation) = arena
            .prepare_cells(&shared_values)
            .map_err(publication::native_fault)?;
        let mut entities = Vec::new();
        let mut shared_ids = Vec::new();
        let mut cells = handles.iter().zip(&shared_values).enumerate();
        for param in &code.params {
            if param.shared {
                let (index, (handle, value)) = cells
                    .next()
                    .ok_or_else(|| malformed("a shared packet parameter has no prepared cell"))?;
                let entity = doc::heap::EntityId {
                    family: doc::heap::FamilyId::PROVISIONAL,
                    ordinal: first
                        .checked_add(index as u64)
                        .ok_or_else(|| malformed("packet parameter identity overflow"))?,
                };
                installation
                    .bind_entity(*handle, entity)
                    .map_err(publication::native_fault)?;
                entities.push(VirtualEntity {
                    entity,
                    body: VirtualBody::NativeVariable(*value),
                });
                shared_ids.push((*handle, entity));
                bindings[param.at as usize] = SlotState::Cell(*handle);
            }
        }
        for (at, state) in bindings.iter().enumerate() {
            if matches!(state, SlotState::Empty) {
                continue;
            }
            let output = source
                .binding_root(code.slots_by_at[at], task, invocation)
                .ok_or_else(|| malformed("packet entry binding has no source owner"))?;
            let value = match state {
                SlotState::Empty => unreachable!(),
                SlotState::Cell(cell)
                    if matches!(
                        output,
                        Owner::Value {
                            slot: lash_kernel_state::kernel_internals::logical::Slot::Capture(_),
                            ..
                        }
                    ) =>
                {
                    PV::Capture(*cell)
                }
                SlotState::Cell(cell) => {
                    let (_, entity) = shared_ids
                        .iter()
                        .find(|(handle, _)| handle == cell)
                        .ok_or_else(|| {
                            malformed("packet entry cell is not a prepared parameter")
                        })?;
                    PV::Page(doc::heap::PageValue::Ref(doc::heap::Identity::Object(
                        *entity,
                    )))
                }
                SlotState::Value(value) => {
                    let parameter = code
                        .params
                        .iter()
                        .position(|param| param.at as usize == at)
                        .ok_or_else(|| malformed("packet entry value is not a parameter"))?;
                    arguments
                        .get(parameter)
                        .and_then(|(owner, _)| *owner)
                        .map_or(PV::Native(*value), PV::Existing)
                }
            };
            outputs.push((output, value));
        }
        Ok(Parameters {
            installation,
            bindings,
            outputs,
            entities,
            shared: !shared_values.is_empty(),
        })
    }
}
