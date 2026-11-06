//! Bind source root lifetimes and amount inputs to compact physical indexes.
use super::super::{code::Register, *};
use crate::compile::region_isa::{CheckedKind, IndexedAmount, Pc, RegionOp, ResourceInput, Role};
use crate::compile::room::Measure;
use lash_kernel_state::kernel_internals::logical::Slot;

#[derive(Clone, Copy)]
pub(super) struct Root {
    pub(super) register: Register,
    pub(super) capture: bool,
    /// A session cell root counts only while the lease holds that cell.
    pub(super) session: Option<usize>,
}
pub(super) enum Fact {
    Local(Register),
    Result,
    /// The native call's contract balance: an adopted answer's own, or for
    /// an answer in place, the returned value's occurrence.
    Native(Balance),
    Release(usize, u64),
    Zero,
    Source(Measure),
}
#[derive(Clone, Copy)]
pub(super) enum Balance {
    Peak,
    Delta,
    Scratch,
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
    /// The session cell this step writes: it releases the cell's held value
    /// and holds the written one.
    pub(super) session: Option<usize>,
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
            | RegionOp::Raise { dst, .. }
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
            RegionOp::Heap { dst: Some(dst), .. }
            | RegionOp::Effect {
                departure: Some(dst),
                ..
            } => Some(dst.index()),
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
                session: None,
            })
        };
        let mut releases: Vec<Root> = before
            .roles
            .iter()
            .filter(|(slot, reg)| {
                Some(reg.index()) == destination
                    || !after.roles.iter().any(|(actual, _)| actual == slot)
            })
            .map(|(slot, reg)| root(*slot, *reg))
            .collect::<Result<_, RegionError>>()?;
        let mut holds: Vec<Root> = after
            .roles
            .iter()
            .filter(|(slot, reg)| {
                Some(reg.index()) == destination
                    || !before.roles.iter().any(|(actual, _)| actual == slot)
            })
            .map(|(slot, reg)| root(*slot, *reg))
            .collect::<Result<_, RegionError>>()?;
        if before.graph.incoming() != after.graph.incoming() {
            if let Some(reg) = before.graph.incoming() {
                releases.push(Root {
                    register: super::bind::physical(code, reg)?,
                    capture: false,
                    session: None,
                });
            }
            if let Some(reg) = after.graph.incoming() {
                holds.push(Root {
                    register: super::bind::physical(code, reg)?,
                    capture: false,
                    session: None,
                });
            }
        }
        let native = match code.op(pc) {
            Some(RegionOp::IntBinary { kernel, .. }) => Some(kernel.function()),
            Some(RegionOp::Checked { kernel, .. }) => match kernel.kind() {
                CheckedKind::Native { function, .. }
                | CheckedKind::IntTruth { function, .. }
                | CheckedKind::FloatBinary { function, .. } => Some(*function),
                _ => None,
            },
            _ => None,
        };
        // The slot each register of the step's before state holds, the
        // first role naming it winning.
        let held: std::collections::HashMap<u32, Slot> = before
            .roles
            .iter()
            .rev()
            .map(|(slot, reg)| (reg.index(), *slot))
            .collect();
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
                                            if let Some(index) =
                                                super::bind::session_index(code, reg.index())
                                            {
                                                return Some(
                                                    super::bind::physical(code, *reg).map(
                                                        |register| Root {
                                                            register,
                                                            capture: false,
                                                            session: Some(index),
                                                        },
                                                    ),
                                                );
                                            }
                                            held.get(&reg.index())
                                                .map(|slot| root(*slot, *reg))
                                        })
                                        .collect::<Result<_, RegionError>>()?;
                                    let index = groups.len();
                                    groups.push(regs.clone());
                                    quotes.push(quote);
                                    index
                                }
                            };
                            Fact::Release(index, 0)
                        }
                        ResourceInput::SourceFact(Measure::Local(
                            crate::compile::room::Owner::Incoming(_),
                        )) => Fact::Local(super::bind::physical(
                            code,
                            before
                                .graph
                                .incoming()
                                .ok_or_else(|| invalid(Refused::Facts))?,
                        )?),
                        ResourceInput::SourceFact(Measure::CallPeak { at, callee })
                            if *at == step.at.site && matches!(callee,
                                crate::compile::room::CallSource::Native(function) if Some(*function) == native) => Fact::Native(Balance::Peak),
                        ResourceInput::SourceFact(Measure::CallDelta { at, callee })
                            if *at == step.at.site && matches!(callee,
                                crate::compile::room::CallSource::Native(function) if Some(*function) == native) => Fact::Native(Balance::Delta),
                        ResourceInput::SourceFact(Measure::NativeScratch { at, function })
                            if *at == step.at.site && Some(*function) == native => Fact::Native(Balance::Scratch),
                        ResourceInput::SourceFact(Measure::Release(owners)) => {
                            let (roots, headers) = release_roots(code, before, owners)?;
                            let index = quotes.len();
                            quotes.push(roots);
                            Fact::Release(index, headers)
                        }
                        ResourceInput::SourceFact(Measure::NativeRelease { at, callee, owners })
                            if *at == step.at.site && matches!(callee,
                                crate::compile::room::CallSource::Native(function) if Some(*function) == native) => {
                            let (roots, headers) = release_roots(code, before, owners)?;
                            let index = quotes.len();
                            quotes.push(roots);
                            Fact::Release(index, headers)
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
                CheckedKind::Local { slot, .. } => Some(super::bind::physical(
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
        let session = destination.and_then(|index| super::bind::session_index(code, index));
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
            holds: holds.into(),
            releases: releases.into(),
            quotes: quotes.into(),
            peak,
            delta,
            local,
            destination,
            session,
        })
    }
    pub(super) fn width(&self) -> usize {
        let session = usize::from(self.session.is_some());
        (self.holds.len() + session)
            .max(self.releases.len() + session)
            .max(
                self.quotes
                    .iter()
                    .map(|quote| quote.len())
                    .max()
                    .unwrap_or(0),
            )
    }
}

fn release_roots(
    code: &RegionCode,
    before: &crate::compile::region_isa::RecoveryMap,
    owners: &[crate::compile::room::Owner],
) -> Result<(Box<[Root]>, u64), RegionError> {
    use lash_kernel_state::kernel_internals::logical::Owner;
    let slots = code.source().owner_slots().map_err(invalid)?;
    let mut roots = std::collections::BTreeSet::new();
    for owner in owners {
        if let Some(root) = slots.root(owner, lash_kernel_doc::TaskId::MAIN, 0) {
            roots.insert(root);
        } else {
            roots.extend(
                slots
                    .group(owner, lash_kernel_doc::TaskId::MAIN, 0)
                    .ok_or_else(|| invalid(Refused::Facts))?
                    .iter()
                    .copied(),
            );
        }
    }
    let mut headers = 0_u64;
    let mut quote = Vec::new();
    for owner in roots {
        let (register, capture, session) = match owner {
            Owner::Frame { .. } => {
                headers = headers
                    .checked_add(lash_kernel_doc::LogicalHeldV1::FRAME)
                    .ok_or_else(|| invalid(Refused::Facts))?;
                continue;
            }
            Owner::Incoming(_) => (
                before
                    .graph
                    .incoming()
                    .ok_or_else(|| invalid(Refused::Facts))?,
                false,
                None,
            ),
            Owner::Value { slot, .. } => {
                let Some((_, reg)) = before.roles.iter().find(|(actual, _)| *actual == slot) else {
                    continue;
                };
                (*reg, matches!(slot, Slot::Capture(_)), None)
            }
            Owner::Session(digest) => {
                let reg = code
                    .roles()
                    .iter()
                    .find_map(|(role, reg)| (*role == Role::Session(digest)).then_some(*reg))
                    .ok_or_else(|| invalid(Refused::Facts))?;
                (
                    reg,
                    false,
                    Some(
                        super::bind::session_index(code, reg.index())
                            .ok_or_else(|| invalid(Refused::Facts))?,
                    ),
                )
            }
            _ => return Err(invalid(Refused::Facts)),
        };
        quote.push(Root {
            register: super::bind::physical(code, register)?,
            capture,
            session,
        });
    }
    Ok((quote.into(), headers))
}
