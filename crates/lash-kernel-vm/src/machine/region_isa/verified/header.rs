//! A protocol `for`'s header rows (`K-ITER-005`): each takes what a hook
//! returned or steps the loop's own cursor, and branches on what `open`
//! gave.
use super::super::{operands::Operand, *};
use super::call_bind::GuardArguments;
use super::control::LoopState;
use super::{Running, scalar::ScalarAnswer};
use crate::compile::region_isa::{CheckedKind, LoopPhase, LoopStage, Pc, Reg};
use lash_kernel_doc::ValueKind;
use lash_kernel_state::kernel_internals::logical::{LeaseFault, LeaseMember, LeaseValue};

/// What a header row does.
pub(super) enum Stood {
    /// The row is taken: its answer, and the loop's state after it.
    Taken(ScalarAnswer, u32, LoopState),
    /// The row is not taken: the header goes on at `Pc`, the loop's state
    /// as given.
    Passed(Pc, u32, LoopState),
    /// What a hook returned is not what the protocol promises.
    Refused(&'static str),
    /// The row reads what only the machine can.
    Boundary,
}

/// What `open` gave: walked by the loop's own cursor, or a driver.
fn walkable(kind: ValueKind) -> bool {
    matches!(
        kind,
        ValueKind::List | ValueKind::Tuple | ValueKind::Map | ValueKind::Set | ValueKind::Text
    )
}

impl Running<'_> {
    fn via_state(&self, info: u32) -> Result<LoopState, RegionError> {
        self.loops
            .get(self.loop_index(info)?)
            .copied()
            .filter(|state| state.active)
            .ok_or_else(|| invalid(Refused::Facts))
    }

    fn absent(&mut self) -> Result<LeaseValue, RegionError> {
        self.view.import(&Value::Absent).map_err(RegionError::Heap)
    }

    fn written(
        &self,
        dst: Reg<LeaseValue>,
        value: LeaseValue,
        next: Pc,
    ) -> Result<ScalarAnswer, RegionError> {
        Ok(ScalarAnswer {
            write: Some((
                super::bind::physical(&self.code.program, dst)?,
                Operand::Dynamic(value),
            )),
            next,
            formula: None,
            native: None,
        })
    }

    pub(super) fn header_answer(
        &mut self,
        kind: &CheckedKind,
        args: &[Reg<LeaseValue>],
        dst: Reg<LeaseValue>,
        next: Pc,
    ) -> Result<Stood, RegionError> {
        match self.header_step(kind, args, dst, next) {
            Ok(stood) => Ok(stood),
            Err(Fault::Boundary) => Ok(Stood::Boundary),
            Err(Fault::Region(error)) => Err(error),
        }
    }

    fn header_step(
        &mut self,
        kind: &CheckedKind,
        args: &[Reg<LeaseValue>],
        dst: Reg<LeaseValue>,
        next: Pc,
    ) -> Result<Stood, Fault> {
        let program = Arc::clone(&self.code.program);
        let arg = |at: usize| args.get(at).copied().ok_or_else(|| invalid(Refused::Facts));
        let none = |next| ScalarAnswer {
            write: None,
            next,
            formula: None,
            native: None,
        };
        match kind {
            CheckedKind::LoopItem { at } => {
                let info = program.loop_at(at).ok_or_else(|| invalid(Refused::Facts))?;
                let mut state = self.via_state(info)?;
                state.started = state
                    .started
                    .checked_add(1)
                    .ok_or_else(|| invalid(Refused::Facts))?;
                let value = self.value(arg(0)?)?;
                let answer = self.written(dst, value, next)?;
                Ok(Stood::Taken(answer, info, state))
            }
            CheckedKind::Header {
                stage: LoopStage { info, phase },
                driver,
                ended,
            } => {
                let info = *info;
                let via = program
                    .loop_info(info)
                    .and_then(|info| info.via)
                    .ok_or_else(|| invalid(Refused::Facts))?;
                match phase {
                    // `open` gave what the loop walks.
                    LoopPhase::Opened => {
                        let opened = self.value(arg(0)?)?;
                        let kind = self.view.kind(opened)?;
                        if !walkable(kind) && !via.next {
                            return Ok(Stood::Refused(
                                "the loop's protocol opened a value it has no `next` to step",
                            ));
                        }
                        let guard = if matches!(kind, ValueKind::Map | ValueKind::Set) && via.check
                        {
                            let size = self.view.member_count(self.view.object(opened)?)?;
                            Some(GuardArguments {
                                size,
                                remaining: size,
                                more: false,
                            })
                        } else {
                            None
                        };
                        let state = LoopState {
                            active: true,
                            guard,
                            ..LoopState::default()
                        };
                        let answer = self.written(dst, opened, next)?;
                        Ok(Stood::Taken(answer, info, state))
                    }
                    // The loop's own cursor steps, or a driver goes to
                    // `next`.
                    LoopPhase::Step => {
                        let mut state = self.via_state(info)?;
                        let walked = self.value(arg(0)?)?;
                        let kind = self.view.kind(walked)?;
                        if !walkable(kind) {
                            let driver = driver.ok_or_else(|| invalid(Refused::Facts))?;
                            return Ok(Stood::Passed(driver, info, state));
                        }
                        let item = match kind {
                            ValueKind::List | ValueKind::Tuple => {
                                let object = self.view.object(walked)?;
                                let item = self
                                    .view
                                    .lookup(object, &LeaseMember::List(state.position))?;
                                if item.is_some() {
                                    state.position += 1;
                                }
                                item
                            }
                            ValueKind::Text => {
                                let item = self.view.text_point(walked, state.position)?;
                                if item.is_some() {
                                    state.position += 1;
                                }
                                item
                            }
                            _ => {
                                let object = self.view.object(walked)?;
                                let found = self.view.table_next(object, state.last)?;
                                if let Some((sequence, _)) = found {
                                    state.last = Some(sequence);
                                }
                                found.map(|(_, key)| key)
                            }
                        };
                        if let Some(guard) = &mut state.guard {
                            guard.more = item.is_some();
                        }
                        match item {
                            Some(item) => {
                                let answer = self.written(dst, item, next)?;
                                Ok(Stood::Taken(answer, info, state))
                            }
                            // A checked loop checks once more at exhaustion.
                            None if state.guard.is_some() => {
                                let absent = self.absent()?;
                                let answer = self.written(dst, absent, next)?;
                                Ok(Stood::Taken(answer, info, state))
                            }
                            None => {
                                state.active = false;
                                let ended = ended.ok_or_else(|| invalid(Refused::Facts))?;
                                Ok(Stood::Passed(ended, info, state))
                            }
                        }
                    }
                    // `next` gave an empty tuple or a tuple of one item.
                    LoopPhase::Pulled => {
                        let state = self.via_state(info)?;
                        let pulled = self.value(arg(0)?)?;
                        let refused =
                            "a loop protocol's `next` gives an empty tuple or a tuple of one item";
                        if self.view.kind(pulled)? != ValueKind::Tuple {
                            return Ok(Stood::Refused(refused));
                        }
                        let object = self.view.object(pulled)?;
                        let ended = ended.ok_or_else(|| invalid(Refused::Facts))?;
                        match self.view.member_count(object)? {
                            0 => {
                                let absent = self.absent()?;
                                let answer = self.written(dst, absent, ended)?;
                                Ok(Stood::Taken(answer, info, state))
                            }
                            1 => {
                                let item = self
                                    .view
                                    .lookup(object, &LeaseMember::List(0))?
                                    .ok_or_else(|| invalid(Refused::Facts))?;
                                let answer = self.written(dst, item, next)?;
                                Ok(Stood::Taken(answer, info, state))
                            }
                            _ => Ok(Stood::Refused(refused)),
                        }
                    }
                    // `check` gave the items the loop may still give.
                    LoopPhase::Checked => {
                        let mut state = self.via_state(info)?;
                        let budget = self.value(arg(0)?)?;
                        let budget = match self.view.inline(budget)? {
                            Some(Value::Int(word)) => u64::try_from(word).ok(),
                            _ => None,
                        };
                        let Some(budget) = budget else {
                            return Ok(Stood::Refused(
                                "a loop protocol's `check` gives a count of the items still allowed",
                            ));
                        };
                        let guard = state
                            .guard
                            .as_mut()
                            .ok_or_else(|| invalid(Refused::Facts))?;
                        guard.remaining = budget;
                        let held = self.value(arg(1)?)?;
                        let present = !matches!(self.view.inline(held)?, Some(Value::Absent));
                        let target = if present {
                            next
                        } else {
                            ended.ok_or_else(|| invalid(Refused::Facts))?
                        };
                        Ok(Stood::Taken(none(target), info, state))
                    }
                    // The loop is left: it goes to its `else` or past it.
                    LoopPhase::Ended => {
                        let mut state = self.via_state(info)?;
                        state.active = false;
                        Ok(Stood::Taken(none(next), info, state))
                    }
                    LoopPhase::Enter | LoopPhase::Test => Err(invalid(Refused::Facts).into()),
                }
            }
            _ => Err(invalid(Refused::Facts).into()),
        }
    }
}

/// Why a header row cannot answer: it reads what only the machine can, or
/// the region is not admitted.
enum Fault {
    Boundary,
    Region(RegionError),
}

impl From<LeaseFault> for Fault {
    fn from(fault: LeaseFault) -> Self {
        match fault {
            LeaseFault::BoundaryRequired => Self::Boundary,
            fault => Self::Region(storage_fault(fault)),
        }
    }
}

impl From<RegionError> for Fault {
    fn from(error: RegionError) -> Self {
        Self::Region(error)
    }
}

impl From<HeapFault> for Fault {
    fn from(fault: HeapFault) -> Self {
        Self::Region(RegionError::Heap(fault))
    }
}
