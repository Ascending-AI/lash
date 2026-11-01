//! Entry consumes evaluated argument occurrences only after its journal accepts.
use super::*;

pub(in crate::machine::registers) struct EntryInputs {
    values: Vec<Input>,
    arguments: usize,
}
struct Input {
    reg: Reg,
    owner: Option<Owner>,
    value: Value,
    kind: Kind,
}
enum Kind {
    Operand,
    Entry(usize),
    Atom(usize),
    Control,
}
impl EntryInputs {
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
    pub(in crate::machine::registers) fn snapshots(
        &self,
    ) -> impl Iterator<Item = (Reg, Value)> + '_ {
        self.values.iter().map(|input| (input.reg, input.value))
    }
}
impl Operands {
    pub(in crate::machine::registers) fn prepare_entry(
        &mut self,
        start: usize,
        count: usize,
        callee: Option<Reg>,
        control: bool,
        registers: &[Value],
    ) -> Result<Option<EntryInputs>, HeapFault> {
        let mut values = Vec::new();
        values
            .try_reserve(count + usize::from(callee.is_some()))
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
        }))
    }
    pub(in crate::machine::registers) fn publish_entry(&mut self, inputs: EntryInputs) {
        for input in inputs.values {
            match input.kind {
                Kind::Operand => {
                    let operand = self
                        .remove_value(input.reg)
                        .expect("exclusively prepared entry operand");
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
                Kind::Control => {}
            }
        }
    }
}
