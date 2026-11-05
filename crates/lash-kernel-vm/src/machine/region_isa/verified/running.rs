//! Private production state uses A's tables and D's graph authority directly.
use super::super::*;
use super::Code;
use crate::compile::region_isa::{Pc, PrefixId, Reg};
use lash_kernel_state::kernel_internals::logical::{LeaseValue, StateLease};

pub(in crate::machine::region_isa) struct Running<'m> {
    pub(super) programs: Arc<super::programs::Programs>,
    pub(super) call_values: Vec<LeaseValue>,
    pub(super) call_captures: Vec<LeaseValue>,
    pub(super) arguments: Vec<Value>,
    pub(super) heap_values: Vec<LeaseValue>,
    pub(super) heap_pairs: Vec<(LeaseValue, LeaseValue)>,
    pub(super) loops: Vec<super::control::LoopState>,
    pub(super) parked: Vec<super::calls::Parked>,
    #[expect(clippy::vec_box, reason = "reuse admitted canonical frame allocations")]
    pub(super) canonical_callees: Vec<Box<crate::machine::Frame>>,
    pub(super) departing: Option<super::returns::Departure>,
    /// The origin of a Break/Continue whose scope Leaves are running.
    pub(super) looping: Option<Point>,
    /// Which root session cells the lease holds, and which it wrote.
    pub(super) sessions: Box<[SessionState]>,
    /// The held session values captured when the root frame left.
    pub(super) closed_sessions: Option<SessionValues>,
    pub(super) returning: Option<super::returns::Incoming>,
    /// The step at this point the machine served, with a native's answer,
    /// taken by that step alone.
    pub(super) served: Option<(Point, Option<Value>)>,
    pub(super) complete: bool,
    pub(super) activation: Option<CallActivation>,
    pub(super) identity: Rc<()>,
    pub(super) scratch: super::phase::Scratch,
    pub(super) crossing: Option<super::charge::Crossing>,
    pub(super) refusal: Option<crate::Bound>,
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
/// Held root session cells by index, with their current values.
pub(super) type SessionValues = Box<[(usize, operands::Operand<LeaseValue>)]>;
#[derive(Clone, Copy, Default)]
pub(super) struct SessionState {
    pub(super) held: bool,
    pub(super) written: bool,
}
impl<'m> Running<'m> {
    /// The root frame's code, which alone may own main session cells.
    pub(super) fn root_code(&self) -> &Arc<Code> {
        self.parked
            .first()
            .map_or(&self.code, |parked| &parked.code)
    }
    /// The root frame's current invocation.
    pub(super) fn root_invocation(&self) -> u64 {
        self.parked
            .first()
            .map_or(self.reached.invocation, |parked| parked.reached.invocation)
    }
    /// The held root session cells and their current values.
    pub(super) fn session_values(&self) -> Result<SessionValues, RegionError> {
        if let Some(closed) = &self.closed_sessions {
            return Ok(closed.clone());
        }
        let invocation = self.root_invocation();
        self.root_code()
            .sessions
            .iter()
            .zip(&self.sessions)
            .enumerate()
            .filter(|(_, (_, state))| state.held)
            .map(|(index, (cell, _))| {
                Ok((index, self.registers.read(0, invocation, cell.register)?))
            })
            .collect()
    }
    pub(super) fn frame_index(&self) -> usize {
        self.parked.len()
    }
    pub(super) fn loop_index(&self, index: u32) -> Result<usize, RegionError> {
        if index as usize >= self.programs.loops {
            return Err(invalid(Refused::Facts));
        }
        self.frame_index()
            .checked_mul(self.programs.loops)
            .and_then(|base| base.checked_add(index as usize))
            .ok_or_else(|| invalid(Refused::Facts))
    }
    #[expect(
        clippy::too_many_arguments,
        reason = "all arguments bind independent actual entry facts"
    )]
    pub(super) fn acquire(
        code: Arc<Code>,
        programs: Arc<super::programs::Programs>,
        pc: Pc,
        prefix: PrefixId,
        invocation: u64,
        values: Vec<(Reg<LeaseValue>, Value)>,
        source: source_counters::SourceCounters,
        limits: Limits,
        crossing: Option<super::charge::Crossing>,
        invocations: invocations::Invocations,
        mut view: StateLease<'m>,
        canonical: Box<crate::machine::Frame>,
    ) -> Result<Self, RegionError> {
        if invocation == 0
            || !invocations.follows(invocation)
            || (limits.entry_charge > limits.charge
                && !crossing.as_ref().is_some_and(|crossing| {
                    crossing.authenticates(limits.charge, limits.entry_charge)
                }))
            || limits.entry_balance > limits.memory
            || limits.entry_depth > limits.depth
        {
            return Err(invalid(Refused::Facts));
        }
        let mut registers = registers::Registers::new(programs.registers, 1)?;
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
            .try_reserve_exact(programs.arguments)
            .map_err(|_| invalid(Refused::Scratch))?;
        let mut heap_values = Vec::new();
        heap_values
            .try_reserve_exact(programs.arguments)
            .map_err(|_| invalid(Refused::Scratch))?;
        let mut heap_pairs = Vec::new();
        heap_pairs
            .try_reserve_exact(programs.arguments)
            .map_err(|_| invalid(Refused::Scratch))?;
        let reached = ReachedPrefix {
            program: code.program.code(),
            invocation,
            ordinal: 0,
            occurrence: None,
            prefix: prefix.index() as u32,
            debt: 0,
        };
        let mut loops = super::control::bind(&code.program, &canonical)?.into_vec();
        loops
            .try_reserve(programs.loops.saturating_sub(loops.len()))
            .map_err(|_| invalid(Refused::Scratch))?;
        loops.resize(programs.loops, super::control::LoopState::default());
        let scratch = super::phase::Scratch::new(
            programs
                .transfer_width
                .max(programs.arguments + 1)
                .max(programs.parameters + programs.captures + 1),
            programs.quotes,
        )?;
        let mut call_values = Vec::new();
        call_values
            .try_reserve_exact(programs.parameters.max(programs.arguments))
            .map_err(|_| invalid(Refused::Scratch))?;
        let mut call_captures = Vec::new();
        call_captures
            .try_reserve_exact(programs.captures)
            .map_err(|_| invalid(Refused::Scratch))?;
        Ok(Self {
            call_captures,
            parked: Vec::new(),
            canonical_callees: Vec::new(),
            departing: None,
            looping: None,
            sessions: vec![SessionState::default(); code.sessions.len()].into(),
            closed_sessions: None,
            returning: None,
            served: None,
            complete: false,
            activation: None,
            identity: Rc::new(()),
            call_values,
            programs,
            loops,
            scratch,
            crossing,
            refusal: None,
            arguments,
            heap_values,
            heap_pairs,
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
}
