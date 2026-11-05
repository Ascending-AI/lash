//! Private production state uses A's tables and D's graph authority directly.
use super::super::*;
use super::Code;
use crate::compile::region_isa::{Pc, PrefixId, Reg};
use lash_kernel_state::kernel_internals::logical::{LeaseValue, StateLease};

pub(in crate::machine::region_isa) struct Running<'m> {
    pub(super) arguments: Vec<Value>,
    pub(super) scratch: super::phase::Scratch,
    pub(super) crossing: Option<super::charge::Crossing>,
    pub(in crate::machine::region_isa) view: StateLease<'m>,
    pub(in crate::machine::region_isa) code: Arc<Code>,
    pub(in crate::machine::region_isa) registers: registers::Registers<LeaseValue>,
    pub(in crate::machine::region_isa) pc: Pc,
    pub(in crate::machine::region_isa) prefix: PrefixId,
    pub(in crate::machine::region_isa) reached: ReachedPrefix,
    pub(in crate::machine::region_isa) counters: execute::Counters,
    pub(in crate::machine::region_isa) limits: Limits,
    pub(in crate::machine::region_isa) source: source_counters::SourceCounters,
    pub(in crate::machine::region_isa) invocations: invocations::Invocations,
    pub(in crate::machine::region_isa) attempted: Option<Point>,
    pub(in crate::machine::region_isa) stop: Option<RegionStop>,
    pub(in crate::machine::region_isa) faulted: bool,
    pub(in crate::machine::region_isa) canonical: Box<crate::machine::Frame>,
}
impl<'m> Running<'m> {
    #[expect(
        clippy::too_many_arguments,
        reason = "all arguments bind independent actual entry facts"
    )]
    pub(super) fn acquire(
        code: Arc<Code>,
        pc: Pc,
        prefix: PrefixId,
        invocation: u64,
        values: Vec<(Reg<LeaseValue>, Value)>,
        source: source_counters::SourceCounters,
        limits: Limits,
        invocations: invocations::Invocations,
        mut view: StateLease<'m>,
        canonical: Box<crate::machine::Frame>,
    ) -> Result<Self, RegionError> {
        if invocation == 0
            || !invocations.follows(invocation)
            || limits.entry_charge > limits.charge
            || limits.entry_balance > limits.memory
            || limits.entry_depth > limits.depth
        {
            return Err(invalid(Refused::Facts));
        }
        let mut registers = registers::Registers::new(code.program.registers().len(), 1)?;
        for (reg, value) in values {
            registers.write(
                &mut view,
                0,
                invocation,
                super::bind::physical(&code.program, reg)?,
                operands::Operand::Immediate(value),
            )?;
        }
        let mut arguments = Vec::new();
        arguments
            .try_reserve_exact(code.arguments)
            .map_err(|_| invalid(Refused::Scratch))?;
        let reached = ReachedPrefix {
            program: code.program.code(),
            invocation,
            ordinal: 0,
            occurrence: None,
            prefix: prefix.index() as u32,
            debt: 0,
        };
        let scratch = super::phase::Scratch::new(code.transfer_width, code.quotes)?;
        Ok(Self {
            scratch,
            crossing: None,
            arguments,
            view,
            code,
            registers,
            pc,
            prefix,
            reached,
            counters: execute::Counters {
                charge: limits.entry_charge,
                progress: limits.entry_progress,
                balance: limits.entry_balance,
                peak: limits.entry_balance,
                depth: limits.entry_depth,
                debt: 0,
            },
            limits,
            source,
            invocations,
            attempted: None,
            stop: None,
            faulted: false,
            canonical,
        })
    }

    pub(in crate::machine::region_isa) fn run(
        &mut self,
        stop: StopAt,
    ) -> Result<RegionStop, RegionError> {
        self.execute(stop)
    }
    pub(in crate::machine::region_isa) fn enter_call(
        &mut self,
        _site: u32,
    ) -> Result<CallActivation, RegionError> {
        Err(invalid(Refused::Facts))
    }
    pub(in crate::machine::region_isa) fn leave_call(
        &mut self,
        _activation: CallActivation,
    ) -> Result<(), RegionError> {
        Err(invalid(Refused::Facts))
    }
}
