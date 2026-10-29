//! Values owned by source occurrences, independent of register mirrors.
use crate::compile::{Stmt, Stream};
use crate::machine::logical_held::{BankRoots, Held, Projection};
use lash_kernel_doc::{Value, heap::HeapFault};
use lash_kernel_state::kernel_internals::logical::{Lifecycle, Owner, Slot, SourceValue};
fn invalid(problem: &str) -> HeapFault {
    HeapFault::Malformed(problem.into())
}
use lash_kernel_exec::{Op, Reg};
use lash_kernel_value::sites::Site;
use std::sync::Arc;

#[derive(Default)]
pub(super) struct Operands {
    roots: Option<BankRoots>,
    entry_roots: Option<Lifecycle>,
    values: Vec<Operand>,
    /// Receipts for source operands transferred to a callee invocation.
    transferred: Vec<Operand>,
    positions: Vec<usize>,
    atoms: Vec<Held>,
    entry: Vec<Held>,
    entry_active: bool,
    atom_site: Option<Site>,
    pub(super) active: bool,
}
struct Operand {
    reg: Reg,
    site: Arc<Site>,
    held: Held,
}
impl Operands {
    pub(super) fn context(&mut self, roots: Option<BankRoots>) {
        self.clear();
        self.roots = roots;
    }
    pub(super) fn entry_roots(&mut self, lifecycle: Lifecycle) {
        for (index, held) in self.entry.iter_mut().enumerate() {
            let owner = Owner::Value {
                task: lash_kernel_doc::TaskId::MAIN,
                invocation: 0,
                slot: Slot::Argument(index as u32),
            };
            lifecycle.replace(owner, Some(SourceValue::Value(held.value)));
            held.owner = Some(owner);
        }
        self.entry_roots = Some(lifecycle);
    }
    pub(super) fn take_arguments(&mut self, start: usize, count: usize) -> Vec<Owner> {
        if self.entry_active {
            return self
                .entry
                .iter_mut()
                .skip(start)
                .take(count)
                .filter_map(|held| held.owner.take())
                .collect();
        }
        (start..start + count)
            .filter_map(|at| {
                let operand = self.remove_value(Reg(at as u32))?;
                let owner = operand.held.owner;
                self.transferred.push(operand);
                owner
            })
            .collect()
    }
    pub(super) fn clear(&mut self) {
        for operand in &self.values {
            if let (Some(roots), Some(owner)) = (&self.roots, operand.held.owner) {
                roots.release(owner);
            }
            self.positions[operand.reg.ix()] = usize::MAX;
        }
        if let Some(roots) = &self.roots {
            for held in &self.atoms {
                if let Some(owner) = held.owner {
                    roots.release(owner);
                }
            }
        }
        if let Some(lifecycle) = self.entry_roots.take() {
            for held in &self.entry {
                if let Some(owner) = held.owner {
                    lifecycle.replace(owner, None);
                }
            }
        }
        self.values.clear();
        self.transferred.clear();
        self.atoms.clear();
        self.entry.clear();
        self.entry_active = false;
        self.atom_site = None;
        self.active = false;
    }
    /// The exported entry's ABI arguments are source occurrences before any
    /// statement or physical register window exists. Build all projections
    /// before replacing the previous bank, so a failed projection keeps it.
    pub(super) fn entry(
        &mut self,
        values: &[Value],
        projection: &Projection<'_>,
    ) -> Result<(), HeapFault> {
        let held = values
            .iter()
            .map(|value| projection.hold(*value))
            .collect::<Result<Vec<_>, _>>()?;
        self.clear();
        self.entry = held;
        self.entry_active = true;
        Ok(())
    }
    pub(super) fn owns_values(&self) -> bool {
        self.active || self.entry_active
    }
    pub(super) fn begin(
        &mut self,
        stream: &Stream,
        stmt: &Stmt,
        pc: u32,
        registers: &[Value],
        projection: &Projection<'_>,
    ) -> Result<(), HeapFault> {
        self.clear();
        self.active = true;
        if self.positions.len() < stmt.regs as usize {
            self.positions.resize(stmt.regs as usize, usize::MAX);
        }
        if pc == stmt.ops.start
            || matches!(stmt.form, crate::compile::Form::While { test, .. } if pc == test.start)
        {
            return Ok(());
        }
        let roles = stmt
            .source_roles_at(stream, pc)
            .ok_or_else(|| invalid("source resume has no operand projection"))?;
        for (reg, site) in roles.iter() {
            if let Some(roots) = &self.roots {
                roots.release(roots.continuation(site)?);
            }
            self.put(Operand {
                reg: *reg,
                site: Arc::clone(site),
                held: projection.hold(
                    *registers
                        .get(reg.ix())
                        .ok_or_else(|| invalid("source resume operand is missing"))?,
                )?,
            })?;
        }
        Ok(())
    }
    pub(super) fn begin_atoms(&mut self, site: &Site) -> Result<(), HeapFault> {
        if !self.active || self.atom_site.is_some() {
            return Err(invalid("action atoms have no vacant source owner"));
        }
        self.atom_site = Some(site.clone());
        Ok(())
    }
    pub(super) fn atom(
        &mut self,
        index: usize,
        value: Value,
        projection: &Projection<'_>,
    ) -> Result<(), HeapFault> {
        if self.atom_site.is_none() || self.atoms.len() != index {
            return Err(invalid("action atom has no ordered source role"));
        }
        let mut held = projection.hold(value)?;
        if let Some(roots) = &self.roots {
            let owner = roots.atom(
                self.atom_site
                    .as_ref()
                    .ok_or_else(|| invalid("source atom has no site"))?,
                index as u32,
            )?;
            roots.acquire(owner, value);
            held.owner = Some(owner);
        }
        self.atoms.push(held);
        Ok(())
    }

    pub(super) fn complete(
        &mut self,
        stream: &Stream,
        pc: u32,
        registers: &[Value],
        projection: &Projection<'_>,
    ) -> Result<(), HeapFault> {
        let op = stream.ops[pc as usize];
        if !self.active {
            return Err(invalid("source operation has no active owner bank"));
        }
        if matches!(op, Op::Region { .. } | Op::Pin { .. }) {
            return Ok(());
        }
        if matches!(op, Op::End) {
            self.clear();
            return Ok(());
        }
        if matches!(op, Op::Act) {
            if let Some(roots) = &self.roots {
                for held in &self.atoms {
                    if let Some(owner) = held.owner {
                        roots.release(owner);
                    }
                }
            }
            self.atoms.clear();
            self.atom_site = None;
        }
        let range = match op {
            Op::MakeTuple { items }
            | Op::MakeList { items }
            | Op::MakeSet { items }
            | Op::MakeMap { entries: items }
            | Op::MakeRecord { values: items, .. }
            | Op::Call { args: items, .. }
            | Op::Operator { args: items, .. }
            | Op::Nested { args: items, .. } => Some(items),
            _ => None,
        };
        match range {
            Some(range) => {
                for reg in range.regs() {
                    self.consume(reg, stream, pc)?;
                }
            }
            None => {
                for reg in op.reads() {
                    self.consume(reg, stream, pc)?;
                }
            }
        }
        if let Some(reg) = op.writes() {
            self.put(Operand {
                reg,
                site: Arc::clone(
                    stream.source_sites[pc as usize]
                        .as_ref()
                        .ok_or_else(|| invalid("source output has no site"))?,
                ),
                held: projection.hold(registers[reg.ix()])?,
            })?;
        }
        if let Some(joins) = stream.source_joins.get(&pc) {
            for join in joins {
                let at = *self
                    .positions
                    .get(join.value.ix())
                    .ok_or_else(|| invalid("source join has no register"))?;
                let operand = self
                    .values
                    .get_mut(at)
                    .ok_or_else(|| invalid("source join has no operand"))?;
                if operand.site != join.from {
                    return Err(invalid("source join names another operand"));
                }
                if let Some(roots) = &self.roots {
                    if let Some(owner) = operand.held.owner {
                        roots.release(owner);
                    }
                    let owner = roots.operand(&join.to)?;
                    roots.acquire(owner, operand.held.value);
                    operand.held.owner = Some(owner);
                }
                operand.site = Arc::clone(&join.to);
            }
        }
        Ok(())
    }
    fn consume(&mut self, reg: Reg, stream: &Stream, pc: u32) -> Result<(), HeapFault> {
        let op = stream.ops[pc as usize];
        if self.remove(reg).is_some() {
            return Ok(());
        }
        // A protocol open enters its callee before ForIn completes. Its
        // iterable was transferred there, so completion spends that exact
        // receipt rather than releasing the former source owner again.
        if matches!(op, Op::ForIn { iterable } if iterable == reg)
            && let Some(at) = self
                .transferred
                .iter()
                .position(|operand| operand.reg == reg)
        {
            let transferred = self.transferred.swap_remove(at);
            let expected = self
                .roots
                .as_ref()
                .map(|roots| roots.operand(&transferred.site))
                .transpose()?;
            if transferred.held.owner == expected {
                return Ok(());
            }
            return Err(invalid("source transfer names another owner"));
        }
        Err(invalid(&format!(
            "source input has no owner: op {op:?}, pc {pc}, register {reg:?}, source {:?}",
            stream.source_sites.get(pc as usize),
        )))
    }

    fn put(&mut self, mut operand: Operand) -> Result<(), HeapFault> {
        let at = self
            .positions
            .get_mut(operand.reg.ix())
            .ok_or_else(|| invalid("source output register is missing"))?;
        if *at != usize::MAX {
            return Err(invalid("source output overwrites an unconsumed operand"));
        }
        *at = self.values.len();
        if let Some(roots) = &self.roots {
            let owner = roots.operand(&operand.site)?;
            roots.acquire(owner, operand.held.value);
            operand.held.owner = Some(owner);
        }
        self.values.push(operand);
        Ok(())
    }
    fn remove(&mut self, reg: Reg) -> Option<Operand> {
        let operand = self.remove_value(reg)?;
        if let (Some(roots), Some(owner)) = (&self.roots, operand.held.owner) {
            roots.release(owner);
        }
        Some(operand)
    }
    fn remove_value(&mut self, reg: Reg) -> Option<Operand> {
        let at = *self.positions.get(reg.ix())?;
        if at == usize::MAX {
            return None;
        }
        self.positions[reg.ix()] = usize::MAX;
        let value = self.values.swap_remove(at);
        if let Some(moved) = self.values.get(at) {
            self.positions[moved.reg.ix()] = at;
        }
        Some(value)
    }
    pub(super) fn take(&mut self, reg: Reg, site: &Site) -> Result<Held, HeapFault> {
        let at = *self
            .positions
            .get(reg.ix())
            .ok_or_else(|| invalid("continuation register is missing"))?;
        let operand = self
            .values
            .get(at)
            .ok_or_else(|| invalid("continuation has no source operand at its role"))?;
        if operand.site.as_ref() != site {
            return Err(invalid("continuation names another source operand"));
        }
        let mut held = self
            .remove(reg)
            .ok_or_else(|| invalid("continuation source operand disappeared"))?
            .held;
        if let Some(roots) = &self.roots {
            let owner = roots.continuation(site)?;
            roots.acquire(owner, held.value);
            held.owner = Some(owner);
        }
        Ok(held)
    }

    pub(super) fn each_value(&self, visit: &mut dyn FnMut(&Value)) {
        for operand in &self.values {
            visit(&operand.held.value);
        }
        self.atoms
            .iter()
            .chain(&self.entry)
            .for_each(|atom| visit(&atom.value));
    }
    pub(super) fn bytes(&self) -> Result<u64, HeapFault> {
        self.values
            .iter()
            .map(|value| &value.held)
            .chain(&self.atoms)
            .chain(&self.entry)
            .try_fold(0_u64, |sum, held| {
                sum.checked_add(held.holding.bytes())
                    .ok_or_else(|| invalid("source operand holding cost overflow"))
            })
    }
    #[cfg(test)]
    pub(super) fn is_empty(&self) -> bool {
        self.values.is_empty() && self.atoms.is_empty() && self.entry.is_empty()
    }
}

#[cfg(test)]
mod laws;
