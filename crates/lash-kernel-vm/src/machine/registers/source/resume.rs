use super::*;
use crate::machine::registers::ResumeValue;

impl Operands {
    /// The ledger retains live caller roots while the callee adopts its bank.
    /// An active callee's own bank or a foreign task's bank cannot be replaced.
    pub(in crate::machine::registers) fn replay_can_replace(
        &self,
        ledger: &lash_kernel_state::kernel_internals::logical::Ledger,
        heap: &lash_kernel_state::kernel_internals::PagedHeap,
        current: &lash_kernel_state::kernel_internals::logical::Invocation,
    ) -> Result<bool, HeapFault> {
        let Owner::Frame {
            task: current_task,
            invocation: current_id,
        } = current.frame()
        else {
            unreachable!("an invocation owns its frame");
        };
        ledger.invocation(current_task, current_id, heap.pages())?;
        for held in self
            .values
            .iter()
            .map(|operand| &operand.held)
            .chain(&self.atoms)
            .chain(&self.entry)
        {
            let Some(owner) = held.owner else { continue };
            if ledger.holding(owner, heap.pages())?.is_none() {
                continue;
            }
            let Owner::Value {
                task, invocation, ..
            } = owner
            else {
                return Ok(false);
            };
            if task != current_task
                || invocation == 0
                || invocation >= current_id
                || ledger
                    .holding(Owner::Frame { task, invocation }, heap.pages())?
                    .is_none()
            {
                return Ok(false);
            }
        }
        Ok(true)
    }
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
