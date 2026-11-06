use super::code::{Cost, Fee, HeapKind};
use super::storage::{Facts, Operation, Ready, Selector, Step, View};
use super::*;
use lash_kernel_state::kernel_internals::logical::LeaseFault;

impl<V: View> Execution<V> {
    pub(super) fn heap_step(
        &mut self,
        site: u32,
        prefix: u32,
        next: u32,
        cost: Cost,
    ) -> Result<(), RegionError> {
        let action = self
            .code
            .heap
            .get(site as usize)
            .ok_or_else(|| invalid(Refused::Facts))?;
        self.arguments.clear();
        for &reg in &action.args {
            let value = self
                .read(reg)?
                .bind(&mut self.view)
                .map_err(RegionError::Heap)?;
            self.arguments.push(value);
        }
        let args = &self.arguments;
        let operation = match action.kind {
            HeapKind::List => Operation::List(args),
            HeapKind::Member(index) if args.len() == 2 => Operation::Member {
                object: args[0],
                member: Selector::List(index),
                next: Some(args[1]),
            },
            HeapKind::Field(index) if args.len() == 2 => Operation::Member {
                object: args[0],
                member: Selector::Field(Arc::clone(
                    self.code
                        .fields
                        .get(index as usize)
                        .ok_or_else(|| invalid(Refused::Facts))?,
                )),
                next: Some(args[1]),
            },
            HeapKind::Grow(additional) if args.len() == 1 => Operation::Grow {
                object: args[0],
                additional,
            },
            HeapKind::Hold(count) if args.len() == 1 => Operation::Hold {
                value: args[0],
                count,
            },
            HeapKind::Release(count) if args.len() == 1 => Operation::Release {
                value: args[0],
                count,
            },
            _ => return Err(invalid(Refused::Facts)),
        };
        let quoted = match cost.fee {
            Fee::Storage { .. } => None,
            _ => Some(if self.limits.charge_enabled {
                self.fee(cost.fee)?
            } else {
                0
            }),
        };
        let stage = match self.view.stage(operation) {
            Ok(stage) => stage,
            Err(LeaseFault::BoundaryRequired) => {
                self.stop = Some(RegionStop::Boundary);
                return Ok(());
            }
            Err(error) => return Err(storage_fault(error)),
        };
        let fee = if !self.limits.charge_enabled {
            0
        } else {
            match cost.fee {
                Fee::Storage {
                    selector,
                    base,
                    per_unit,
                } => {
                    let amount = stage
                        .facts()
                        .measure(selector)
                        .ok_or_else(|| invalid(Refused::Facts))?;
                    let Some(fee) = amount
                        .checked_mul(per_unit)
                        .and_then(|amount| amount.checked_add(base))
                    else {
                        drop(stage);
                        self.attempted = Some(
                            self.code
                                .recovery
                                .get(prefix as usize)
                                .ok_or_else(|| invalid(Refused::Exit))?
                                .continuation
                                .clone(),
                        );
                        self.stop = Some(RegionStop::Refusal);
                        return Err(invalid(Refused::Peak));
                    };
                    fee
                }
                _ => quoted.ok_or_else(|| invalid(Refused::Facts))?,
            }
        };
        let cost = Cost {
            delta: stage.facts().delta(),
            peak: stage.facts().peak(),
            ..cost
        };
        let Some(counters) = Self::budget(self.counters, self.limits, cost, fee) else {
            drop(stage);
            self.attempted = Some(
                self.code
                    .recovery
                    .get(prefix as usize)
                    .ok_or_else(|| invalid(Refused::Exit))?
                    .continuation
                    .clone(),
            );
            self.stop = Some(RegionStop::Refusal);
            return Err(invalid(Refused::Peak));
        };
        let ready = stage.reserve().map_err(RegionError::Heap)?;
        if ready.result().is_some() != action.destination.is_some() {
            return Err(invalid(Refused::Facts));
        }
        let map = self
            .code
            .recovery
            .get(prefix as usize)
            .ok_or_else(|| invalid(Refused::Exit))?;
        let ordinal = self
            .reached
            .ordinal
            .checked_add(1)
            .ok_or_else(|| invalid(Refused::Facts))?;
        let frame_index = self
            .frames
            .len()
            .checked_sub(1)
            .ok_or_else(|| invalid(Refused::Facts))?;
        let frame = self
            .frames
            .get_mut(frame_index)
            .ok_or_else(|| invalid(Refused::Facts))?;
        let destination = action
            .destination
            .map(|index| {
                self.registers.prepare_dynamic(
                    frame_index,
                    frame.invocation,
                    code::Register::Value(index),
                )
            })
            .transpose()?;
        let source = self.source.prepare(&map.counters)?;
        let value = ready.accept();
        if let Some(destination) = destination {
            destination.publish(value);
        }
        let occurrence = source.accept();
        frame.pc = next;
        frame.prefix = prefix;
        self.reached = ReachedPrefix {
            program: frame.program,
            invocation: frame.invocation,
            ordinal,
            occurrence,
            prefix,
            debt: counters.debt,
        };
        self.counters = counters;
        Ok(())
    }
}
