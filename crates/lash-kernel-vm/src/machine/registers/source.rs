//! Values owned by source occurrences, independent of register mirrors.
use crate::compile::{Stmt, Stream};
use lash_kernel_doc::Value;
use lash_kernel_exec::{Op, Reg};
use lash_kernel_value::sites::Site;
use std::sync::Arc;

#[derive(Default)]
pub(super) struct Operands {
    values: Vec<Operand>,
    positions: Vec<usize>,
    pub(super) active: bool,
}
struct Operand {
    reg: Reg,
    site: Arc<Site>,
    value: Value,
}
impl Operands {
    pub(super) fn clear(&mut self) {
        for operand in &self.values {
            self.positions[operand.reg.ix()] = usize::MAX;
        }
        self.values.clear();
        self.active = false;
    }
    pub(super) fn begin(
        &mut self,
        stream: &Stream,
        stmt: &Stmt,
        pc: u32,
        registers: &[Value],
    ) -> Result<(), &'static str> {
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
            .ok_or("source resume has no operand projection")?;
        for (reg, site) in roles.iter() {
            self.put(Operand {
                reg: *reg,
                site: Arc::clone(site),
                value: *registers
                    .get(reg.ix())
                    .ok_or("source resume operand is missing")?,
            })?;
        }
        Ok(())
    }
    pub(super) fn complete(
        &mut self,
        stream: &Stream,
        pc: u32,
        registers: &[Value],
    ) -> Result<(), &'static str> {
        let op = stream.ops[pc as usize];
        if !self.active {
            return Err("source operation has no active owner bank");
        }
        if matches!(op, Op::Region { .. } | Op::Pin { .. }) {
            return Ok(());
        }
        if matches!(op, Op::End) {
            self.clear();
            return Ok(());
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
                    self.remove(reg).ok_or("source input has no owner")?;
                }
            }
            None => {
                for reg in op.reads() {
                    self.remove(reg).ok_or("source input has no owner")?;
                }
            }
        }
        if let Some(reg) = op.writes() {
            self.put(Operand {
                reg,
                site: Arc::clone(
                    stream.source_sites[pc as usize]
                        .as_ref()
                        .ok_or("source output has no site")?,
                ),
                value: registers[reg.ix()],
            })?;
        }
        if let Some(joins) = stream.source_joins.get(&pc) {
            for join in joins {
                let at = *self
                    .positions
                    .get(join.value.ix())
                    .ok_or("source join has no register")?;
                let operand = self
                    .values
                    .get_mut(at)
                    .ok_or("source join has no operand")?;
                if operand.site != join.from {
                    return Err("source join names another operand");
                }
                operand.site = Arc::clone(&join.to);
            }
        }
        Ok(())
    }
    fn put(&mut self, operand: Operand) -> Result<(), &'static str> {
        let at = self
            .positions
            .get_mut(operand.reg.ix())
            .ok_or("source output register is missing")?;
        if *at != usize::MAX {
            return Err("source output overwrites an unconsumed operand");
        }
        *at = self.values.len();
        self.values.push(operand);
        Ok(())
    }
    fn remove(&mut self, reg: Reg) -> Option<Operand> {
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
    pub(super) fn take(&mut self, reg: Reg, site: &Site) -> Result<Value, &'static str> {
        let at = *self
            .positions
            .get(reg.ix())
            .ok_or("continuation register is missing")?;
        let operand = self
            .values
            .get(at)
            .ok_or("continuation has no source operand at its role")?;
        if operand.site.as_ref() != site {
            return Err("continuation names another source operand");
        }
        Ok(self
            .remove(reg)
            .ok_or("continuation source operand disappeared")?
            .value)
    }

    pub(super) fn each_value(&self, visit: &mut dyn FnMut(&Value)) {
        for operand in &self.values {
            visit(&operand.value);
        }
    }
    #[cfg(test)]
    pub(super) fn is_empty(&self) -> bool {
        self.values.is_empty()
    }
}

#[cfg(test)]
mod laws;
