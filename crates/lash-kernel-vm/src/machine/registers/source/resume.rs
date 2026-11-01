use super::*;
use crate::machine::registers::ResumeValue;

impl Operands {
    pub(in crate::machine::registers) fn prepare_resumption(
        &self,
        roots: BankRoots,
        extent: usize,
        values: Vec<ResumeValue>,
    ) -> Result<Option<Self>, HeapFault> {
        if !self.values.is_empty()
            || self
                .atoms
                .iter()
                .chain(&self.entry)
                .any(|held| held.owner.is_some())
        {
            return Ok(None);
        }
        let mut next = Self {
            roots: Some(roots),
            active: true,
            ..Default::default()
        };
        next.positions
            .try_reserve(extent)
            .map_err(|_| HeapFault::Exhausted {
                resource: lash_kernel_doc::heap::HostResource::StagingBytes,
            })?;
        next.positions.resize(extent, usize::MAX);
        next.values
            .try_reserve(values.len())
            .map_err(|_| HeapFault::Exhausted {
                resource: lash_kernel_doc::heap::HostResource::StagingBytes,
            })?;
        for value in values {
            let owner = next
                .roots
                .as_ref()
                .ok_or_else(|| HeapFault::Malformed("a resumed source bank has no roots".into()))?
                .operand(&value.site)?;
            if value.reg.ix() >= extent
                || next.positions[value.reg.ix()] != usize::MAX
                || value.held.owner != Some(owner)
            {
                return Err(invalid("resumption bank names another source operand"));
            }
            next.positions[value.reg.ix()] = next.values.len();
            next.values.push(Operand {
                reg: value.reg,
                site: value.site,
                held: value.held,
            });
        }
        Ok(Some(next))
    }
    pub(in crate::machine::registers) fn resumption_matches(&self, values: &[Value]) -> bool {
        self.active
            && self.values.iter().all(|operand| {
                values
                    .get(operand.reg.ix())
                    .is_some_and(|value| value.same(&operand.held.value))
            })
    }
}
