//! Verified storage descriptors execute once inside the exclusive lease.
use super::super::*;
use super::{Running, charge};
use crate::compile::region_isa::Role;
use crate::compile::region_isa::{HeapKernel, HeapKind, Pc, Reg};
use crate::compile::room::charges::region_isa::ChargeMoment;
use lash_kernel_state::kernel_internals::logical::{
    LeaseAllocation, LeaseFault, LeaseMember, LeaseOp, LeaseValue,
};

fn counters(
    before: Counters,
    delta: i128,
    peak: u64,
    limiters: &mut crate::machine::limits::Limiters,
    limits: &mut Limits,
) -> Result<Option<Counters>, RegionError> {
    let balance = i128::from(before.balance)
        .checked_add(delta)
        .and_then(|balance| u64::try_from(balance).ok())
        .ok_or_else(|| invalid(Refused::Facts))?;
    let peak = before
        .balance
        .checked_add(peak)
        .ok_or_else(|| invalid(Refused::Peak))?
        .max(balance)
        .max(before.peak);
    Ok(
        charge::fits_memory(limiters, limits, peak).then_some(Counters {
            balance,
            peak,
            ..before
        }),
    )
}

impl Running<'_> {
    pub(super) fn heap_step(
        &mut self,
        kernel: &HeapKernel,
        args: &[Reg<LeaseValue>],
        dst: Option<Reg<LeaseValue>>,
        next: Pc,
    ) -> Result<bool, RegionError> {
        let code = Arc::clone(&self.code);
        let step = code
            .program
            .step(self.pc)
            .ok_or_else(|| invalid(Refused::Path))?;
        self.heap_values.clear();
        self.heap_pairs.clear();
        for reg in args {
            let value = self.value(*reg)?;
            self.heap_values.push(value);
        }
        self.scratch.releases.clear();
        if !kernel.atomic_move() {
            let flow = code
                .flows
                .get(self.pc.index())
                .and_then(Option::as_ref)
                .ok_or_else(|| invalid(Refused::Facts))?;
            for root in &flow.releases {
                let value = self
                    .registers
                    .read(self.parked.len(), self.reached.invocation, root.register)?
                    .bind(&mut self.view)
                    .map_err(RegionError::Heap)?;
                self.scratch.releases.push((value, root.capture));
            }
        }
        let member = matches!(
            kernel.kind(),
            HeapKind::Field { .. } | HeapKind::Index { .. } | HeapKind::Variable { .. }
        );
        let scratch = if member {
            self.heap_values.iter().try_fold(0_u64, |bytes, value| {
                bytes
                    .checked_add(self.view.price(*value).map_err(RegionError::Heap)?)
                    .ok_or_else(|| invalid(Refused::Scratch))
            })?
        } else {
            0
        };
        let closure_identity = if let HeapKind::Closure {
            code: target,
            captures,
        } = kernel.kind()
        {
            self.call_captures.clear();
            for slot in captures {
                let reg = code
                    .program
                    .roles()
                    .iter()
                    .find_map(|(actual, reg)| (*actual == Role::Slot(*slot)).then_some(*reg))
                    .ok_or_else(|| invalid(Refused::Facts))?;
                let value = self.value(reg)?;
                self.call_captures.push(value);
            }
            let target = code.exe.code(*target);
            let lash_kernel_value::sites::Unit::Library(function) = target.site.unit else {
                return Err(invalid(Refused::Source));
            };
            Some(Arc::new(lash_kernel_doc::arena::ClosureIdentity {
                capture_names: target.capture_names.clone(),
                callable: lash_kernel_value::names::CallableId {
                    link: self.frame_link(),
                    function,
                },
                site: lash_kernel_doc::linked_site::LinkedSite {
                    link: self.frame_link(),
                    site: target
                        .created
                        .clone()
                        .ok_or_else(|| invalid(Refused::Source))?,
                },
            }))
        } else {
            None
        };
        // A list position past the end, which the state declines to stage.
        let mut past_end = None;
        let operation = match kernel.kind() {
            HeapKind::Field { name, removing } => {
                let base = *self
                    .heap_values
                    .first()
                    .ok_or_else(|| invalid(Refused::Facts))?;
                if self.view.kind(base).map_err(RegionError::Heap)?
                    != lash_kernel_doc::ValueKind::Record
                {
                    self.stop = Some(RegionStop::Refused {
                        kind: "type_error",
                        message: if *removing {
                            "only a record has fields to remove"
                        } else {
                            "only a record has fields to assign"
                        }
                        .into(),
                    });
                    return Ok(false);
                }
                let object = self.view.object(base).map_err(RegionError::Heap)?;
                let next = if *removing {
                    None
                } else {
                    Some(
                        *self
                            .heap_values
                            .get(1)
                            .ok_or_else(|| invalid(Refused::Facts))?,
                    )
                };
                LeaseOp::Member {
                    object,
                    member: LeaseMember::Field(Arc::clone(name)),
                    next,
                }
            }
            HeapKind::Index { removing } => {
                let base = *self
                    .heap_values
                    .first()
                    .ok_or_else(|| invalid(Refused::Facts))?;
                let key = *self
                    .heap_values
                    .get(1)
                    .ok_or_else(|| invalid(Refused::Facts))?;
                let object = self.view.object(base).map_err(RegionError::Heap)?;
                let kind = self.view.kind(base).map_err(RegionError::Heap)?;
                // A set's member is assigned `true` to add it, held with no
                // value, or `false` to remove it, as the source does.
                let mut removing = *removing;
                if kind == lash_kernel_doc::ValueKind::Set && !removing {
                    let assigned = *self
                        .heap_values
                        .get(2)
                        .ok_or_else(|| invalid(Refused::Facts))?;
                    match self.view.inline(assigned).map_err(RegionError::Heap)? {
                        Some(Value::Bool(true)) => {
                            self.heap_values[2] = self
                                .view
                                .import(&Value::Absent)
                                .map_err(RegionError::Heap)?;
                        }
                        Some(Value::Bool(false)) => removing = true,
                        _ => {
                            self.stop = Some(RegionStop::Refused {
                                kind: "type_error",
                                message: "a set's member is assigned `true` to add it or `false` to remove it".into(),
                            });
                            return Ok(false);
                        }
                    }
                }
                // A member the source cannot write or remove raises as the
                // machine's own write does (`K-VAL-010`).
                let member = match kind {
                    lash_kernel_doc::ValueKind::List => {
                        let index = match self.view.inline(key).map_err(RegionError::Heap)? {
                            Some(Value::Int(word)) => u64::try_from(word).ok(),
                            Some(Value::Float(value)) => {
                                let value = value.get();
                                (value >= 0.0 && value.fract() == 0.0 && value < u64::MAX as f64)
                                    .then_some(value as u64)
                            }
                            _ if self.view.kind(key).map_err(RegionError::Heap)?
                                == lash_kernel_doc::ValueKind::Int =>
                            {
                                None
                            }
                            _ => {
                                self.stop = Some(RegionStop::Refused {
                                    kind: "type_error",
                                    message: "a list is indexed by a number".into(),
                                });
                                return Ok(false);
                            }
                        };
                        let Some(index) = index else {
                            let length = self.view.member_count(object).map_err(storage_fault)?;
                            self.stop = Some(RegionStop::Refused {
                                kind: "index_out_of_range",
                                message: if removing {
                                    format!("a list of {length} has no element there to remove")
                                } else {
                                    format!("a list of {length} is assigned at 0 to {length}")
                                }
                                .into(),
                            });
                            return Ok(false);
                        };
                        past_end = Some((object, removing));
                        LeaseMember::List(index)
                    }
                    lash_kernel_doc::ValueKind::Record
                        if self.view.kind(key).map_err(RegionError::Heap)?
                            != lash_kernel_doc::ValueKind::Text =>
                    {
                        self.stop = Some(RegionStop::Refused {
                            kind: "type_error",
                            message: "a record index must be text".into(),
                        });
                        return Ok(false);
                    }
                    lash_kernel_doc::ValueKind::Map
                    | lash_kernel_doc::ValueKind::Set
                    | lash_kernel_doc::ValueKind::Record => LeaseMember::Table(key),
                    _ => {
                        self.stop = Some(RegionStop::Refused {
                            kind: "type_error",
                            message: if removing {
                                "only a list, a record, a map or a set has an index to remove"
                            } else {
                                "only a list, a record, a map or a set is assigned by index"
                            }
                            .into(),
                        });
                        return Ok(false);
                    }
                };
                let next = if removing {
                    None
                } else {
                    Some(
                        *self
                            .heap_values
                            .get(2)
                            .ok_or_else(|| invalid(Refused::Facts))?,
                    )
                };
                LeaseOp::Member {
                    object,
                    member,
                    next,
                }
            }
            HeapKind::Variable { slot } => {
                let register = code
                    .program
                    .roles()
                    .iter()
                    .find_map(|(actual, reg)| (*actual == Role::Slot(*slot)).then_some(*reg))
                    .ok_or_else(|| invalid(Refused::Facts))?;
                let base = self.value(register)?;
                let object = self.view.object(base).map_err(RegionError::Heap)?;
                let [value] = self.heap_values.as_slice() else {
                    return Err(invalid(Refused::Facts));
                };
                LeaseOp::Member {
                    object,
                    member: LeaseMember::Variable,
                    next: Some(*value),
                }
            }
            _ => {
                let allocation = match kernel.kind() {
                    HeapKind::Closure { .. } => LeaseAllocation::closure(
                        closure_identity.ok_or_else(|| invalid(Refused::Facts))?,
                        &self.call_captures,
                    ),
                    HeapKind::List => LeaseAllocation::list(&self.heap_values),
                    HeapKind::Tuple => LeaseAllocation::tuple(&self.heap_values),
                    HeapKind::Set => LeaseAllocation::set(&self.heap_values),
                    HeapKind::Map => {
                        let (pairs, remainder) = self.heap_values.as_chunks::<2>();
                        self.heap_pairs
                            .extend(pairs.iter().map(|pair| (pair[0], pair[1])));
                        if !remainder.is_empty() {
                            return Err(invalid(Refused::Facts));
                        }
                        LeaseAllocation::map(&self.heap_pairs)
                    }
                    HeapKind::Record(fields) => {
                        LeaseAllocation::record_fields(fields, &self.heap_values)
                    }
                    HeapKind::Cell => {
                        let [value] = self.heap_values.as_slice() else {
                            return Err(invalid(Refused::Facts));
                        };
                        LeaseAllocation::cell(value)
                    }
                    HeapKind::Text(text) => LeaseAllocation::text(text),
                    HeapKind::Bytes(bytes) => LeaseAllocation::bytes(bytes),
                    HeapKind::BigInt(integer) => LeaseAllocation::big_int(integer),
                    HeapKind::Function(function) => {
                        LeaseAllocation::function(lash_kernel_value::names::CallableId {
                            link: self.frame_link(),
                            function: *function,
                        })
                    }
                    _ => return Err(invalid(Refused::Facts)),
                };
                if kernel.atomic_move() {
                    // A authenticates the exact operand list, including multiplicity.
                    if !kernel.source_moves().iter().eq(args) {
                        return Err(invalid(Refused::Facts));
                    }
                    LeaseOp::AllocateMove {
                        allocation,
                        releases: &self.heap_values,
                    }
                } else {
                    LeaseOp::Allocate(allocation)
                }
            }
        };
        // A step the state refuses with a typed error (a key that is no
        // key) raises it before the step is billed, as a served step does.
        let phase = match self.view.stage(operation) {
            Ok(phase) => phase,
            Err(LeaseFault::BoundaryRequired) => {
                self.stop = Some(RegionStop::Boundary);
                return Ok(false);
            }
            Err(LeaseFault::Native(lash_kernel_doc::NativeError::Refused { kind, message })) => {
                self.stop = Some(RegionStop::Refused {
                    kind,
                    message: message.into(),
                });
                return Ok(false);
            }
            Err(LeaseFault::Native(lash_kernel_doc::NativeError::Decline))
                if let Some((object, removing)) = past_end =>
            {
                let length = self.view.member_count(object).map_err(storage_fault)?;
                self.stop = Some(RegionStop::Refused {
                    kind: "index_out_of_range",
                    message: if removing {
                        format!("a list of {length} has no element there to remove")
                    } else {
                        format!("a list of {length} is assigned at 0 to {length}")
                    }
                    .into(),
                });
                return Ok(false);
            }
            Err(error) => return Err(storage_fault(error)),
        };
        let entry = self.counters;
        let payment = charge::billed(self.limiters, &mut self.limits, |limits| {
            charge::bill(entry, limits, &step.charges, ChargeMoment::After, None)
        })?;
        #[cfg(any(test, feature = "testing"))]
        self.observed_charges.extend_from_slice(&payment.events);
        self.counters = payment.counters;
        if let Some(crossing) = payment.crossing {
            self.refusal = Some(crate::Bound::Charge);
            self.crossing = Some(crossing);
            self.attempted = Some(step.at.clone());
            self.stop = Some(RegionStop::Refusal);
            return Ok(false);
        }
        let Some(allocated) = counters(
            self.counters,
            phase.facts().delta(),
            scratch
                .checked_add(phase.facts().peak())
                .ok_or_else(|| invalid(Refused::Peak))?,
            self.limiters,
            &mut self.limits,
        )?
        else {
            self.refusal = Some(crate::Bound::Memory);
            self.attempted = Some(step.at.clone());
            self.stop = Some(RegionStop::Refusal);
            return Ok(false);
        };
        let write = dst
            .map(|destination| {
                // A heap result lands in a frame value register; a session
                // cell is written only through a priced flow.
                let register = super::bind::physical(&code.program, destination)?;
                if !matches!(register, code::Register::Value(_)) {
                    return Err(invalid(Refused::Facts));
                }
                self.registers
                    .prepare_dynamic(self.parked.len(), self.reached.invocation, register)
            })
            .transpose()?;
        let ready = phase.reserve().map_err(RegionError::Heap)?;
        if ready.result().is_some() != write.is_some() {
            return Err(invalid(Refused::Facts));
        }
        let value = ready.accept();
        if let Some(write) = write {
            write.publish(value);
        }
        self.counters = allocated;
        if !kernel.atomic_move() {
            let phase = self
                .view
                .stage(LeaseOp::Transfer {
                    holds: &[],
                    releases: &self.scratch.releases,
                })
                .map_err(storage_fault)?;
            let released = counters(
                self.counters,
                phase.facts().delta(),
                phase.facts().peak(),
                self.limiters,
                &mut self.limits,
            )?
            .ok_or_else(|| invalid(Refused::Peak))?;
            phase.reserve().map_err(RegionError::Heap)?.accept();
            self.counters = released;
        }
        self.counters.debt = self
            .counters
            .debt
            .checked_add(1)
            .ok_or_else(|| invalid(Refused::Facts))?;
        self.reached.ordinal = self
            .reached
            .ordinal
            .checked_add(1)
            .ok_or_else(|| invalid(Refused::Facts))?;
        self.prefix = step.after;
        self.reached.prefix = step.after.index() as u32;
        self.reached.debt = self.counters.debt;
        self.pc = next;
        Ok(true)
    }
}
