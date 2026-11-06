//! A `finally` suspends the exit that leaves its try while its cleanup runs,
//! and restores it once the cleanup completes (`K-FORM-017`). The suspended
//! exit is the frame control record's own: its value is a dynamically owned
//! root there, moved with no occurrence of its own.
use super::super::{operands::Operand, *};
use super::Running;
use super::returns::Departure;
use super::scalar::ScalarAnswer;
use crate::compile::region_isa::{CheckedKind, Pc, Reg};
use crate::compile::room::ExitKind;
use lash_kernel_state::kernel_internals::logical::{LeaseValue, Owner, Slot};
use lash_kernel_value::sites::Site;

/// An exit its try's cleanup suspended.
pub(super) struct Suspended {
    /// The frame, by depth, whose control record holds it.
    pub(super) frame: usize,
    pub(super) at: Site,
    pub(super) exit: ExitKind,
    pub(super) origin: Point,
    /// A Return's quoted completion debt, kept as it was quoted.
    pub(super) departure: Option<Departure>,
    /// A Return's or a raise's value, and the frame slot that owns it:
    /// the try's pending departure.
    pub(super) value: Option<(Slot, LeaseValue)>,
}

/// What a finally row changes once its step is accepted.
pub(super) enum Finally {
    Suspend(Box<Suspended>),
    /// The departure leaving a cleanup drops what its try suspended; its
    /// value's root is released by the step.
    Supersede,
    Restore,
}

impl Running<'_> {
    /// The frame slot of the try at `at`'s pending departure.
    pub(super) fn pending_slot(&self, at: &Site) -> Result<Slot, RegionError> {
        let slots = self.code.program.source().owner_slots().map_err(invalid)?;
        match slots.root(
            &crate::compile::room::Owner::PendingDeparture(at.clone()),
            lash_kernel_doc::TaskId::MAIN,
            0,
        ) {
            Some(Owner::Value {
                slot: slot @ Slot::Departure(_),
                ..
            }) => Ok(slot),
            _ => Err(invalid(Refused::Facts)),
        }
    }

    /// The exits suspended in the frame at `depth`, outermost first.
    pub(super) fn suspended_in(&self, depth: usize) -> impl Iterator<Item = &Suspended> {
        self.suspended
            .iter()
            .filter(move |entry| entry.frame == depth)
    }

    /// The exits suspended in the frame at `depth` as its controls record
    /// them, their values as `roles` holds them.
    pub(super) fn suspensions(
        &self,
        depth: usize,
        roles: &[(Slot, Value)],
    ) -> Result<Vec<super::control::Suspension>, RegionError> {
        self.suspended_in(depth)
            .map(|entry| {
                let value = || {
                    let (slot, _) = entry.value.ok_or_else(|| invalid(Refused::Facts))?;
                    roles
                        .iter()
                        .find_map(|(actual, value)| (*actual == slot).then(|| Box::new(*value)))
                        .ok_or_else(|| invalid(Refused::Facts))
                };
                let completion = match entry.exit {
                    ExitKind::Return => crate::machine::Completion::Return(value()?),
                    ExitKind::Raise => crate::machine::Completion::Throw(value()?),
                    ExitKind::Break => crate::machine::Completion::Break,
                    ExitKind::Continue => crate::machine::Completion::Continue,
                    _ => return Err(invalid(Refused::Facts)),
                };
                Ok((entry.at.clone(), completion))
            })
            .collect()
    }

    /// Take the exits a reopened frame's cleanups suspended, `reopened`
    /// gives them, replaying nothing.
    pub(super) fn reopen_suspended(
        &mut self,
        depth: usize,
        reopened: Vec<Reopened>,
    ) -> Result<(), RegionError> {
        self.suspended
            .try_reserve(reopened.len())
            .map_err(|_| invalid(Refused::Scratch))?;
        for (at, exit, value, origin, completion) in reopened {
            let value = value
                .map(|value| {
                    let bound = Operand::Immediate(value)
                        .bind(&mut self.view)
                        .map_err(RegionError::Heap)?;
                    Ok::<_, RegionError>((self.pending_slot(&at)?, bound))
                })
                .transpose()?;
            let departure = match (exit, value) {
                (ExitKind::Return, Some((_, value))) => Some(Departure {
                    origin: origin.clone(),
                    value,
                    completion,
                }),
                _ => None,
            };
            self.suspended.push(Suspended {
                frame: depth,
                at,
                exit,
                origin,
                departure,
                value,
            });
        }
        Ok(())
    }

    /// The exit the cleanup of the try at `at` suspended in this frame, if
    /// it is the innermost one.
    pub(super) fn suspended_at(&self, at: &Site) -> Option<&Suspended> {
        self.suspended
            .last()
            .filter(|top| top.frame == self.frame_index() && top.at == *at)
    }

    /// Whether a Supersede row goes on unaccepted: its try suspended
    /// nothing.
    pub(super) fn passes(&self, kind: &CheckedKind) -> bool {
        matches!(kind, CheckedKind::Supersede { at } if self.suspended_at(at).is_none())
    }

    /// The value a Supersede row releases.
    pub(super) fn superseded(&self) -> Option<LeaseValue> {
        self.suspended
            .last()
            .filter(|top| top.frame == self.frame_index())
            .and_then(|top| top.value)
            .map(|(_, value)| value)
    }

    pub(super) fn finally_answer(
        &mut self,
        kind: &CheckedKind,
        args: &[Reg<LeaseValue>],
        dst: Reg<LeaseValue>,
        next: Pc,
    ) -> Result<(ScalarAnswer, Option<Finally>), RegionError> {
        let none = |next| ScalarAnswer {
            write: None,
            next,
            formula: None,
            native: None,
        };
        match kind {
            CheckedKind::Suspend { at, exit } => {
                let departure = self.departing.as_ref();
                let origin = departure
                    .map(|departure| departure.origin.clone())
                    .or_else(|| self.looping.clone())
                    .ok_or_else(|| invalid(Refused::Facts))?;
                if (*exit == ExitKind::Return) != departure.is_some() {
                    return Err(invalid(Refused::Facts));
                }
                let value = match args {
                    [] => None,
                    [reg] => Some((self.pending_slot(at)?, self.value(*reg)?)),
                    _ => return Err(invalid(Refused::Facts)),
                };
                let suspended = Suspended {
                    frame: self.frame_index(),
                    at: at.clone(),
                    exit: *exit,
                    origin,
                    departure: None,
                    value,
                };
                Ok((none(next), Some(Finally::Suspend(Box::new(suspended)))))
            }
            CheckedKind::Restore { at, exit } | CheckedKind::Escape { at, exit } => {
                let top = self
                    .suspended_at(at)
                    .filter(|top| top.exit == *exit)
                    .ok_or_else(|| invalid(Refused::Facts))?;
                if self.code.program.restore_row(at, &top.origin) != Some(self.pc)
                    || self.departing.is_some()
                    || self.looping.is_some()
                {
                    return Err(invalid(Refused::Facts));
                }
                let write = top
                    .value
                    .map(|(_, value)| {
                        Ok::<_, RegionError>((
                            super::bind::physical(&self.code.program, dst)?,
                            Operand::Dynamic(value),
                        ))
                    })
                    .transpose()?;
                Ok((
                    ScalarAnswer {
                        write,
                        next,
                        formula: None,
                        native: None,
                    },
                    Some(Finally::Restore),
                ))
            }
            CheckedKind::Supersede { at } => {
                if self.suspended_at(at).is_none() {
                    return Err(invalid(Refused::Facts));
                }
                Ok((none(next), Some(Finally::Supersede)))
            }
            // A cleanup completes to the Restore of what it suspended.
            CheckedKind::Cleanup { at } => {
                let next = match self.suspended_at(at) {
                    Some(top) => self
                        .code
                        .program
                        .restore_row(at, &top.origin)
                        .ok_or_else(|| invalid(Refused::Facts))?,
                    None => next,
                };
                Ok((none(next), None))
            }
            _ => Err(invalid(Refused::Facts)),
        }
    }

    /// Take an accepted finally row's change: a Suspend moves the frame's
    /// departure into its control record, a Restore moves it back.
    pub(super) fn accept_finally(&mut self, change: Finally) -> Result<(), RegionError> {
        match change {
            Finally::Suspend(mut suspended) => {
                suspended.departure = self.departing.take();
                self.looping = None;
                self.suspended
                    .try_reserve(1)
                    .map_err(|_| invalid(Refused::Scratch))?;
                self.suspended.push(*suspended);
            }
            Finally::Supersede => {
                self.suspended
                    .pop()
                    .ok_or_else(|| invalid(Refused::Facts))?;
            }
            Finally::Restore => {
                let top = self
                    .suspended
                    .pop()
                    .ok_or_else(|| invalid(Refused::Facts))?;
                match top.departure {
                    Some(departure) => self.departing = Some(departure),
                    None => self.looping = Some(top.origin),
                }
            }
        }
        Ok(())
    }
}

/// An exit a saved frame's cleanup suspended: its try, its kind, its
/// value, its origin and its quoted completion debt.
pub(super) type Reopened = (Site, ExitKind, Option<Value>, Point, u64);

/// The exits `frame`'s cleanups suspended, outermost first, from its
/// controls and its continuation; each must agree with the other.
pub(super) fn reopened(
    code: &super::Code,
    frame: &crate::machine::Frame,
) -> Result<Vec<Reopened>, RegionError> {
    let saved = frame
        .pending
        .source
        .as_ref()
        .map_or(&[][..], |source| &source.suspended[..]);
    let mut saved = saved.iter();
    let mut reopened = Vec::new();
    for control in &frame.control {
        let crate::machine::Control::Try {
            stmt,
            phase: crate::machine::TryPhase::Finally(completion),
        } = control
        else {
            continue;
        };
        let (exit, value) = match completion {
            crate::machine::Completion::Normal => continue,
            crate::machine::Completion::Break => (ExitKind::Break, None),
            crate::machine::Completion::Continue => (ExitKind::Continue, None),
            crate::machine::Completion::Return(value) => (ExitKind::Return, Some(**value)),
            crate::machine::Completion::Throw(value) => (ExitKind::Raise, Some(**value)),
            crate::machine::Completion::ReturnVia { .. } => {
                return Err(invalid(Refused::Facts));
            }
        };
        let crate::compile::Form::Try { site, .. } = &code.exe.stmt(*stmt).form else {
            return Err(invalid(Refused::Facts));
        };
        let (origin, completion) = saved.next().ok_or_else(|| invalid(Refused::Facts))?;
        if exit != ExitKind::Return && *completion != 0 {
            return Err(invalid(Refused::Facts));
        }
        reopened
            .try_reserve(1)
            .map_err(|_| invalid(Refused::Scratch))?;
        reopened.push((site.clone(), exit, value, origin.clone(), *completion));
    }
    if saved.next().is_some() {
        return Err(invalid(Refused::Facts));
    }
    Ok(reopened)
}
