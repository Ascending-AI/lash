//! Values owned by source occurrences, independent of register mirrors.
use crate::compile::{Stmt, Stream};
use lash_kernel_doc::Value;
use lash_kernel_exec::{Op, Reg};
use lash_kernel_value::sites::Site;
use std::sync::Arc;

#[derive(Default)]
pub(super) struct Operands {
    values: Vec<Operand>,
    pub(super) active: bool,
}
struct Operand {
    reg: Reg,
    site: Arc<Site>,
    value: Value,
}
impl Operands {
    pub(super) fn clear(&mut self) {
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
        if pc == stmt.ops.start
            || matches!(stmt.form, crate::compile::Form::While { test, .. } if pc == test.start)
        {
            return Ok(());
        }
        let roles = stmt
            .source_roles_at(stream, pc)
            .ok_or("source resume has no operand projection")?;
        for (reg, site) in roles {
            self.values.push(Operand {
                reg: *reg,
                site: Arc::clone(site),
                value: *registers
                    .get(reg.ix())
                    .ok_or("source resume operand is missing")?,
            });
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
        if matches!(op, Op::Region { .. } | Op::Pin { .. }) {
            return Ok(());
        }
        let reads = op.reads();
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
        self.values.retain(|operand| !match range {
            Some(range) => {
                operand.reg.0 >= range.start.0 && operand.reg.0 - range.start.0 < range.len
            }
            None => reads.contains(&operand.reg),
        });
        if let Some(reg) = op.writes() {
            if self.values.iter().any(|operand| operand.reg == reg) {
                return Err("source output overwrites an unconsumed operand");
            }
            self.values.push(Operand {
                reg,
                site: Arc::clone(
                    stream.source_sites[pc as usize]
                        .as_ref()
                        .ok_or("source output has no site")?,
                ),
                value: registers[reg.ix()],
            });
        }
        if let Some(joins) = stream.source_joins.get(&pc) {
            for join in joins {
                let operand = self
                    .values
                    .iter_mut()
                    .find(|operand| operand.reg == join.value)
                    .ok_or("source join has no operand")?;
                if operand.site != join.from {
                    return Err("source join names another operand");
                }
                operand.site = Arc::clone(&join.to);
            }
        }
        Ok(())
    }
    pub(super) fn take(&mut self, reg: Reg, site: &Site) -> Result<Value, &'static str> {
        let at = self
            .values
            .iter()
            .position(|operand| operand.reg == reg && operand.site.as_ref() == site)
            .ok_or("continuation has no source operand at its role")?;
        Ok(self.values.remove(at).value)
    }
    #[cfg(test)]
    pub(super) fn is_empty(&self) -> bool {
        self.values.is_empty()
    }
}

#[cfg(test)]
mod laws;
