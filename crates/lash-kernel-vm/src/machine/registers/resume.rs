//! The accepted resumption owns a prepared bank until either executor adopts it.
use super::*;
use crate::machine::logical_held::BankRoots;
use lash_kernel_state::kernel_internals::logical::{Ledger, Stamp};
use lash_kernel_value::sites::Site;
use std::sync::Arc;

pub(in crate::machine) struct ResumeAt {
    pub(in crate::machine) task: lash_kernel_doc::TaskId,
    pub(in crate::machine) invocation: u64,
    pub(in crate::machine) statement: Site,
    pub(in crate::machine) pc: u32,
}
pub(in crate::machine) struct ResumeValue {
    pub(in crate::machine) reg: lash_kernel_exec::Reg,
    pub(in crate::machine) site: Arc<Site>,
    pub(in crate::machine) held: Held,
}
pub(super) struct Ready {
    at: ResumeAt,
    stamp: Option<Stamp>,
}
pub(in crate::machine) struct PreparedResume<'a> {
    registers: &'a mut Registers,
    source: source::Operands,
    writes: Vec<(usize, Value)>,
    extent: usize,
    at: ResumeAt,
}
impl PreparedResume<'_> {
    pub(in crate::machine) fn publish(self) {
        self.registers.clear_range(0..self.registers.extent);
        self.registers.extent = 0;
        self.registers.kill = 0;
        self.registers.owns_values = false;
        self.registers.window(self.extent);
        for (reg, value) in self.writes {
            self.registers.put(reg, value);
        }
        self.registers.source = self.source;
        self.registers.ready = Some(Ready {
            at: self.at,
            stamp: None,
        });
    }
}
impl Registers {
    pub(in crate::machine) fn prepare_source_resume(
        &mut self,
        at: ResumeAt,
        roots: BankRoots,
        extent: u32,
        values: Vec<ResumeValue>,
    ) -> Result<Option<PreparedResume<'_>>, HeapFault> {
        if self.ready.is_some() {
            return Err(HeapFault::Malformed(
                "an accepted source resumption was replaced".into(),
            ));
        }
        let extent = extent as usize;
        let mut writes = Vec::new();
        writes
            .try_reserve(values.len())
            .map_err(|_| HeapFault::Exhausted {
                resource: lash_kernel_doc::heap::HostResource::StagingBytes,
            })?;
        writes.extend(
            values
                .iter()
                .map(|value| (value.reg.ix(), value.held.value)),
        );
        let Some(source) = self.source.prepare_resumption(roots, extent, values)? else {
            return Ok(None);
        };
        self.values
            .try_reserve(extent.saturating_sub(self.values.len()))
            .map_err(|_| HeapFault::Exhausted {
                resource: lash_kernel_doc::heap::HostResource::StagingBytes,
            })?;
        self.written
            .try_reserve(extent.saturating_sub(self.written.len()))
            .map_err(|_| HeapFault::Exhausted {
                resource: lash_kernel_doc::heap::HostResource::StagingBytes,
            })?;
        Ok(Some(PreparedResume {
            registers: self,
            source,
            writes,
            extent,
            at,
        }))
    }
    pub(in crate::machine) fn bind_source_resume(&mut self, ledger: &Ledger) {
        let Some(ready) = self.ready.as_mut() else {
            unreachable!("the accepted journal installed its bank");
        };
        assert!(ready.stamp.is_none());
        ready.stamp = Some(ledger.stamp());
    }
    fn take_source_resume(
        &mut self,
        task: lash_kernel_doc::TaskId,
        invocation: u64,
        statement: &Site,
        pc: u32,
        ledger: &Ledger,
    ) -> Result<bool, HeapFault> {
        let Some(ready) = &self.ready else {
            return Ok(false);
        };
        if ready.at.task != task
            || ready.at.invocation != invocation
            || &ready.at.statement != statement
            || ready.at.pc != pc
            || ready
                .stamp
                .as_ref()
                .is_none_or(|stamp| !ledger.is_current(stamp))
            || !self.source.resumption_matches(&self.values)
        {
            return Err(HeapFault::Malformed(
                "accepted source resumption changed before adoption".into(),
            ));
        }
        self.ready = None;
        Ok(true)
    }
    pub(in crate::machine) fn begin_source_resume(
        &mut self,
        task: lash_kernel_doc::TaskId,
        invocation: u64,
        statement: &Site,
        pc: u32,
        ledger: &Ledger,
    ) -> Result<bool, HeapFault> {
        self.take_source_resume(task, invocation, statement, pc, ledger)
    }
}
