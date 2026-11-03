//! Entry consumes evaluated argument occurrences only after its journal accepts.
use super::*;

#[derive(Clone)]
pub(in crate::machine::registers) struct EntryInputs {
    values: Vec<Input>,
    arguments: usize,
    call_len: usize,
    public: Option<Vec<(Option<Owner>, Value)>>,
}
#[derive(Clone)]
struct Input {
    reg: Reg,
    owner: Option<Owner>,
    value: Value,
    kind: Kind,
    saved: Option<SavedInput>,
}
#[derive(Clone)]
struct SavedInput {
    site: crate::compile::Suffix,
    holding: lash_kernel_state::kernel_internals::logical::Holding,
    destination: Owner,
}
#[derive(Clone)]
enum Kind {
    Operand,
    Entry(usize),
    Atom(usize),
    Control,
    Adapted,
}
pub(super) struct ExportEntry {
    start: usize,
    callee: Option<Reg>,
    control: bool,
    public: EntryInputs,
    private: Vec<Value>,
}
impl EntryInputs {
    pub(in crate::machine::registers) fn public_arguments(
        &self,
    ) -> Option<&[(Option<Owner>, Value)]> {
        self.public.as_deref()
    }

    pub(in crate::machine::registers) fn arguments(
        &self,
    ) -> impl Iterator<Item = (Option<Owner>, Value)> + '_ {
        self.values[..self.arguments]
            .iter()
            .map(|input| (input.owner, input.value))
    }
    pub(in crate::machine::registers) fn owners(&self) -> impl Iterator<Item = Owner> + '_ {
        self.values.iter().filter_map(|input| input.owner)
    }
    pub(in crate::machine::registers) fn call_owners(&self) -> impl Iterator<Item = Owner> + '_ {
        self.values[..self.call_len]
            .iter()
            .filter_map(|input| input.owner)
    }
    pub(in crate::machine::registers) fn saved(
        &self,
    ) -> impl Iterator<
        Item = (
            &crate::compile::Suffix,
            Value,
            &lash_kernel_state::kernel_internals::logical::Holding,
            Owner,
            Owner,
        ),
    > + '_ {
        self.values.iter().filter_map(|input| {
            let saved = input.saved.as_ref()?;
            Some((
                &saved.site,
                input.value,
                &saved.holding,
                input.owner?,
                saved.destination,
            ))
        })
    }
    pub(in crate::machine::registers) fn snapshots(
        &self,
    ) -> impl Iterator<Item = (Reg, Value)> + '_ {
        self.values.iter().map(|input| (input.reg, input.value))
    }
}
impl Operands {
    pub(super) fn take_export_arguments(
        &mut self,
        start: usize,
        count: usize,
    ) -> Option<Vec<Owner>> {
        let entry = self.export.as_ref()?;
        if entry.start != start || entry.private.len() != count {
            return None;
        }
        let entry = self.export.take()?;
        let owners = entry.public.owners().collect();
        self.publish_entry(entry.public);
        Some(owners)
    }

    pub(in crate::machine::registers) fn stage_export(
        &mut self,
        start: usize,
        callee: Option<Reg>,
        control: bool,
        public: EntryInputs,
    ) -> Result<(), HeapFault> {
        if self.export.is_some() {
            return Err(invalid("export arguments already prepared"));
        }
        self.export = Some(ExportEntry {
            start,
            callee,
            control,
            public,
            private: Vec::new(),
        });
        Ok(())
    }
    pub(in crate::machine::registers) fn finish_export(
        &mut self,
        start: usize,
        values: Vec<Value>,
    ) -> Result<(), HeapFault> {
        let entry = self
            .export
            .as_mut()
            .ok_or_else(|| invalid("export has no public argument proof"))?;
        if entry.start != start {
            return Err(invalid("export arguments moved registers"));
        }
        entry.private = values;
        Ok(())
    }
    pub(in crate::machine::registers) fn public_entry_count(
        &self,
        start: usize,
        count: usize,
    ) -> Result<usize, HeapFault> {
        match &self.export {
            Some(entry) if entry.start == start && entry.private.len() == count => {
                Ok(entry.public.arguments)
            }
            Some(_) => Err(invalid(
                "export entry differs from its prepared private ABI",
            )),
            None => Ok(count),
        }
    }

    pub(in crate::machine::registers) fn prepare_entry(
        &mut self,
        start: usize,
        count: usize,
        callee: Option<Reg>,
        control: bool,
        registers: &[Value],
        roles: &[crate::compile::NestedRole],
    ) -> Result<Option<EntryInputs>, HeapFault> {
        if let Some(entry) = &self.export {
            if entry.start != start
                || entry.private.len() != count
                || entry.callee != callee
                || entry.control != control
                || !entry.private.iter().enumerate().all(|(index, expected)| {
                    registers
                        .get(start + index)
                        .is_some_and(|actual| actual.same(expected))
                })
            {
                return Err(invalid(
                    "export entry changed its authenticated private arguments",
                ));
            }
            let mut inputs = entry.public.clone();
            inputs.public = Some(entry.public.arguments().collect());
            let mut values = Vec::with_capacity(count + inputs.values.len());
            values.extend(
                entry
                    .private
                    .iter()
                    .enumerate()
                    .map(|(index, value)| Input {
                        reg: Reg((start + index) as u32),
                        owner: None,
                        value: *value,
                        kind: Kind::Adapted,
                        saved: None,
                    }),
            );
            values.append(&mut inputs.values);
            inputs.values = values;
            inputs.arguments = count;
            inputs.call_len += count;
            return Ok(Some(inputs));
        }
        let mut values = Vec::new();
        values
            .try_reserve(count + usize::from(callee.is_some()) + roles.len())
            .map_err(|_| HeapFault::Exhausted {
                resource: lash_kernel_doc::heap::HostResource::StagingBytes,
            })?;
        for reg in (start..start + count)
            .map(|at| Reg(at as u32))
            .chain(callee)
        {
            let Some(value) = registers.get(reg.ix()).copied() else {
                return Err(invalid("frame entry has no argument register"));
            };
            let (owner, expected, kind) = if control {
                (None, value, Kind::Control)
            } else if self.entry_active {
                let held = self
                    .entry
                    .get(reg.ix())
                    .ok_or_else(|| invalid("entry argument is missing"))?;
                (held.owner, held.value, Kind::Entry(reg.ix()))
            } else if self.atom_site.is_some() && callee.is_none() {
                let at = reg.ix() - start;
                let held = self
                    .atoms
                    .get(at)
                    .ok_or_else(|| invalid("entry atom is missing"))?;
                (held.owner, held.value, Kind::Atom(at))
            } else {
                let Some(input) = self.prepare_transfer(reg)? else {
                    return Ok(None);
                };
                (Some(input.owner()), input.value(), Kind::Operand)
            };
            if !expected.same(&value) || (!control && owner.is_none()) {
                return Err(invalid(
                    "frame entry argument differs from its source holding",
                ));
            }
            values.push(Input {
                reg,
                owner,
                value,
                kind,
                saved: None,
            });
        }
        let call_len = values.len();
        for role in roles {
            if values.iter().any(|input| input.reg == role.reg) {
                return Err(invalid("entry role overlaps another source input"));
            }
            let Some(input) = self.prepare_transfer(role.reg)? else {
                return Ok(None);
            };
            let at = self.positions[role.reg.ix()];
            let operand = &self.values[at];
            if operand.site.as_ref() != &role.site
                || !registers
                    .get(role.reg.ix())
                    .is_some_and(|value| value.same(&input.value()))
            {
                return Err(invalid(
                    "entry role differs from its evaluated source operand",
                ));
            }
            let destination = self
                .roots
                .as_ref()
                .ok_or_else(|| invalid("entry role has no source roots"))?
                .continuation(&role.site)?;
            values.push(Input {
                reg: role.reg,
                owner: Some(input.owner()),
                value: input.value(),
                kind: Kind::Operand,
                saved: Some(SavedInput {
                    site: Arc::clone(&role.suffix),
                    holding: operand.held.holding.clone(),
                    destination,
                }),
            });
        }
        self.transferred
            .try_reserve(
                values
                    .iter()
                    .filter(|input| matches!(input.kind, Kind::Operand))
                    .count(),
            )
            .map_err(|_| HeapFault::Exhausted {
                resource: lash_kernel_doc::heap::HostResource::StagingBytes,
            })?;
        Ok(Some(EntryInputs {
            values,
            arguments: count,
            call_len,
            public: None,
        }))
    }
    pub(in crate::machine::registers) fn publish_entry(&mut self, inputs: EntryInputs) {
        self.export = None;
        for input in inputs.values {
            match input.kind {
                Kind::Operand => {
                    let Some(operand) = self.remove_value(input.reg) else {
                        unreachable!("exclusively prepared entry operand");
                    };
                    assert!(
                        operand.held.owner == input.owner && operand.held.value.same(&input.value)
                    );
                    self.transferred.push(operand);
                }
                Kind::Entry(at) => {
                    self.entry[at].owner = None;
                }
                Kind::Atom(at) => {
                    self.atoms[at].owner = None;
                }
                Kind::Control | Kind::Adapted => {}
            }
        }
    }
}
