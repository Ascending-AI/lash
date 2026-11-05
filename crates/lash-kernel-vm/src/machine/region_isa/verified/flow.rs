//! Bind source root lifetimes and amount inputs to compact physical indexes.
use super::super::{code::Register, *};
use crate::compile::region_isa::{CheckedKind, IndexedAmount, Pc, RegionOp, ResourceInput};
use crate::compile::room::Measure;
use lash_kernel_state::kernel_internals::logical::Slot;

#[derive(Clone, Copy)]
pub(super) struct Root {
    pub(super) register: Register,
    pub(super) capture: bool,
}
pub(super) enum Fact {
    Local(Register),
    Result,
    Release(usize),
    Source(Measure),
}
pub(super) struct Amount {
    pub(super) fixed: i128,
    pub(super) terms: Box<[(Fact, i128)]>,
}
pub(super) struct Flow {
    pub(super) frame_delta: i128,
    pub(super) loop_branch: Option<u32>,
    pub(super) holds: Box<[Root]>,
    pub(super) releases: Box<[Root]>,
    pub(super) quotes: Box<[Box<[Root]>]>,
    pub(super) peak: Amount,
    pub(super) delta: Amount,
    pub(super) local: Option<Register>,
    pub(super) destination: Option<Register>,
}
impl Flow {
    pub(super) fn bind(code: &RegionCode, pc: Pc) -> Result<Self, RegionError> {
        let step = code.step(pc).ok_or_else(|| invalid(Refused::Source))?;
        let before = code
            .recovery(step.before)
            .ok_or_else(|| invalid(Refused::Exit))?;
        let after = code
            .recovery(step.after)
            .ok_or_else(|| invalid(Refused::Exit))?;
        let destination = match code.op(pc).ok_or_else(|| invalid(Refused::Path))? {
            RegionOp::Constant { dst, .. }
            | RegionOp::Move { dst, .. }
            | RegionOp::Read { dst, .. } => Some(dst.index()),
            RegionOp::IntBinary { dst, .. } => Some(dst.index()),
            RegionOp::Checked { kernel, dst, .. }
                if !matches!(
                    kernel.kind(),
                    CheckedKind::Phase | CheckedKind::LoopControl { .. }
                ) =>
            {
                Some(dst.index())
            }
            RegionOp::Heap { dst: Some(dst), .. } => Some(dst.index()),
            RegionOp::Return { .. } => after
                .roles
                .iter()
                .find_map(|(slot, reg)| matches!(slot, Slot::Departure(_)).then_some(reg.index())),
            _ => None,
        };
        let root = |slot: Slot, reg| {
            Ok(Root {
                register: super::bind::physical(code, reg)?,
                capture: matches!(slot, Slot::Capture(_)),
            })
        };
        let releases = before
            .roles
            .iter()
            .filter(|(slot, reg)| {
                Some(reg.index()) == destination
                    || !after.roles.iter().any(|(actual, _)| actual == slot)
            })
            .map(|(slot, reg)| root(*slot, *reg))
            .collect::<Result<_, RegionError>>()?;
        let holds = after
            .roles
            .iter()
            .filter(|(slot, reg)| {
                Some(reg.index()) == destination
                    || !before.roles.iter().any(|(actual, _)| actual == slot)
            })
            .map(|(slot, reg)| root(*slot, *reg))
            .collect::<Result<_, RegionError>>()?;
        let mut groups = Vec::new();
        let mut quotes = Vec::new();
        let mut amount = |amount: &IndexedAmount| -> Result<Amount, RegionError> {
            let terms = amount
                .terms
                .iter()
                .map(|(input, coefficient)| {
                    let fact = match input {
                        ResourceInput::Local(reg) => {
                            Fact::Local(super::bind::physical(code, *reg)?)
                        }
                        ResourceInput::Result => Fact::Result,
                        ResourceInput::Release(regs) => {
                            let index = match groups.iter().position(|group| group == regs) {
                                Some(index) => index,
                                None => {
                                    let quote = regs
                                        .iter()
                                        .filter_map(|reg| {
                                            before.roles.iter().find_map(|(slot, actual)| {
                                                (actual == reg).then_some((*slot, *reg))
                                            })
                                        })
                                        .map(|(slot, reg)| root(slot, reg))
                                        .collect::<Result<_, RegionError>>()?;
                                    let index = groups.len();
                                    groups.push(regs.clone());
                                    quotes.push(quote);
                                    index
                                }
                            };
                            Fact::Release(index)
                        }
                        ResourceInput::SourceFact(measure) => Fact::Source(measure.clone()),
                    };
                    Ok((fact, *coefficient))
                })
                .collect::<Result<_, RegionError>>()?;
            Ok(Amount {
                fixed: amount.fixed,
                terms,
            })
        };
        let peak = amount(&step.peak)?;
        let delta = amount(&step.delta)?;
        let local = match code.op(pc) {
            Some(RegionOp::Checked { kernel, .. }) => match kernel.kind() {
                CheckedKind::Local { slot } => Some(super::bind::physical(
                    code,
                    code.roles()
                        .iter()
                        .find_map(|(actual, reg)| (actual == slot).then_some(*reg))
                        .ok_or_else(|| invalid(Refused::Facts))?,
                )?),
                _ => None,
            },
            _ => None,
        };
        let destination = destination
            .map(|index| match code.registers().get(index as usize) {
                Some(crate::compile::region_isa::RegisterKind::Value) => Ok(Register::Value(index)),
                Some(crate::compile::region_isa::RegisterKind::Int) => Ok(Register::Int(index)),
                Some(crate::compile::region_isa::RegisterKind::Float) => Ok(Register::Float(index)),
                Some(crate::compile::region_isa::RegisterKind::Bool) => Ok(Register::Bool(index)),
                None => Err(invalid(Refused::Facts)),
            })
            .transpose()?;
        Ok(Self {
            frame_delta: if matches!(code.op(pc), Some(RegionOp::Exit { .. }))
                && step.at.site == code.source().site
                && step.at.phase == crate::compile::room::Phase::Leave
            {
                -i128::from(lash_kernel_doc::LogicalHeldV1::FRAME)
            } else {
                0
            },
            loop_branch: super::control::branch(code, pc),
            holds,
            releases,
            quotes: quotes.into(),
            peak,
            delta,
            local,
            destination,
        })
    }
    pub(super) fn width(&self) -> usize {
        self.holds.len().max(self.releases.len()).max(
            self.quotes
                .iter()
                .map(|quote| quote.len())
                .max()
                .unwrap_or(0),
        )
    }
}
