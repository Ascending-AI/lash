//! Opt-in observations of actual placements for the call/frame laws.

use super::*;
use lash_kernel_doc::Datum;

#[derive(Debug)]
pub(crate) struct Placed {
    pub(crate) role: u32,
    pub(crate) at: usize,
    pub(crate) value: Option<Datum>,
}

#[derive(Debug)]
pub(crate) struct FramePlacement {
    pub(crate) code: u32,
    pub(crate) packet: usize,
    pub(crate) bindings: Range<usize>,
    pub(crate) operands: Range<usize>,
    pub(crate) source_operands: usize,
    pub(crate) nested: bool,
    pub(crate) pending: Range<usize>,
    pub(crate) waiting: bool,
    pub(crate) retained: Vec<Placed>,
    pub(crate) departures: Vec<Placed>,
}

pub(crate) type StackPlacement = Vec<FramePlacement>;

impl Slab {
    pub(in crate::machine::islands) fn trace(&mut self) {
        self.placements = Some(Vec::new());
    }

    pub(in crate::machine::islands) fn take_placements(&mut self) -> Vec<StackPlacement> {
        self.placements
            .as_mut()
            .map(std::mem::take)
            .unwrap_or_default()
    }

    pub(in crate::machine::islands) fn record_placement(
        &mut self,
        callers: &[Frame<'_>],
        active: &Frame<'_>,
        arena: &Arena,
    ) {
        let Some(placements) = &mut self.placements else {
            return;
        };
        let placed = |role, at| {
            let value = match self.values[at] {
                SlotState::Value(value) => datum(arena, value),
                _ => None,
            };
            Placed { role, at, value }
        };
        placements.push(
            callers
                .iter()
                .chain(std::iter::once(active))
                .map(|frame| {
                    let live = frame.continuation.map_or_else(
                        || &*frame.callee.compiled.code.recovery[frame.pc].live,
                        |continuation| continuation.live,
                    );
                    FramePlacement {
                        code: frame.callee.plan.code.0,
                        packet: frame.pc,
                        bindings: frame.bindings.clone(),
                        operands: frame.operands.clone(),
                        source_operands: frame.callee.compiled.registers,
                        nested: frame
                            .continuation
                            .is_some_and(|call| call.site.act.is_some()),
                        pending: frame.pending.clone(),
                        waiting: frame.continuation.is_some(),
                        retained: live
                            .iter()
                            .map(|reg| placed(*reg, frame.operands.start + *reg as usize))
                            .collect(),
                        departures: frame
                            .control
                            .iter()
                            .filter_map(|control| match control {
                                Position::Try {
                                    cleanup,
                                    phase:
                                        Phase::Finally(
                                            Departure::Return
                                            | Departure::ReturnVia(_)
                                            | Departure::Throw,
                                        ),
                                } => Some(placed(
                                    cleanup.id.0,
                                    frame.pending.start + cleanup.pending,
                                )),
                                _ => None,
                            })
                            .collect(),
                    }
                })
                .collect(),
        );
    }
}

fn scalar(value: Value) -> Option<Datum> {
    match value {
        Value::Null => Some(Datum::Null),
        Value::Int(value) => Some(Datum::Int(value.into())),
        Value::Float(value) => Some(Datum::Float(value)),
        Value::Bool(value) => Some(Datum::Bool(value)),
        _ => None,
    }
}

fn datum(arena: &Arena, value: Value) -> Option<Datum> {
    match value {
        Value::List(handle) => Some(Datum::List(
            arena
                .semantics()
                .sequence(handle)
                .ok()?
                .iter()
                .copied()
                .map(scalar)
                .collect::<Option<Vec<_>>>()?,
        )),
        _ => scalar(value),
    }
}
