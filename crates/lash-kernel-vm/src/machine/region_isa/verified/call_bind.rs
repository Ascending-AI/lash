//! Verified parameter/capture substitutions become physical indexes once.
use super::super::{code::Register, *};
use super::Code;
use crate::compile::region_isa::Role;
use crate::compile::room::region::calls::region_isa::CalleePlan;
use crate::compile::room::region::calls::region_isa::callee::{GuardValue, Substitution};
use lash_kernel_state::kernel_internals::logical::Slot;
use lash_kernel_state::kernel_internals::logical::{LeaseValue, StateLease};

pub(super) enum Input {
    Register(Register),
    Absent,
    Guard(GuardValue),
}

/// A protocol `for`'s guard counts, as its `check` takes them.
#[derive(Clone, Copy)]
pub(super) struct GuardArguments {
    pub(super) size: u64,
    pub(super) remaining: u64,
    pub(super) more: bool,
}

pub(super) struct Parameter {
    pub(super) input: Input,
    pub(super) output: Register,
    pub(super) shared: bool,
}

pub(super) struct Target {
    pub(super) code: CodeId,
    pub(super) parameters: Box<[Parameter]>,
    pub(super) captures: Box<[(u32, Register)]>,
}

pub(super) struct Calls {
    sites: Box<[Box<[Target]>]>,
}

impl Target {
    /// Borrow actual caller values before creating a callee holding. Missing
    /// formals receive Absent; shared formals still need their typed cell phase.
    pub(super) fn inputs(
        &self,
        registers: &registers::Registers<LeaseValue>,
        frame: usize,
        invocation: u64,
        lease: &mut StateLease<'_>,
        values: &mut Vec<LeaseValue>,
        guard: Option<GuardArguments>,
    ) -> Result<(), RegionError> {
        if values.capacity() < self.parameters.len() {
            return Err(invalid(Refused::Scratch));
        }
        values.clear();
        for parameter in &self.parameters {
            let value = match parameter.input {
                Input::Register(input) => registers
                    .read(frame, invocation, input)?
                    .bind(lease)
                    .map_err(RegionError::Heap)?,
                Input::Absent => lease.import(&Value::Absent).map_err(RegionError::Heap)?,
                Input::Guard(value) => {
                    let guard = guard.ok_or_else(|| invalid(Refused::Facts))?;
                    let count = |count: u64| {
                        i64::try_from(count)
                            .map(Value::Int)
                            .map_err(|_| invalid(Refused::Facts))
                    };
                    let value = match value {
                        GuardValue::Size => count(guard.size)?,
                        GuardValue::Remaining => count(guard.remaining)?,
                        GuardValue::More => Value::Bool(guard.more),
                    };
                    lease.import(&value).map_err(RegionError::Heap)?
                }
            };
            values.push(value);
        }
        Ok(())
    }
}

fn role(code: &RegionCode, slot: Slot) -> Result<Register, RegionError> {
    let reg = code
        .roles()
        .iter()
        .find_map(|(actual, reg)| (*actual == Role::Slot(slot)).then_some(*reg))
        .ok_or_else(|| invalid(Refused::Facts))?;
    super::bind::physical(code, reg)
}

impl Calls {
    pub(super) fn captures(&self) -> usize {
        self.sites
            .iter()
            .flat_map(|targets| targets.iter())
            .map(|target| target.captures.len())
            .max()
            .unwrap_or(0)
    }
    pub(super) fn width(&self) -> usize {
        self.sites
            .iter()
            .flat_map(|targets| targets.iter())
            .map(|target| target.parameters.len())
            .max()
            .unwrap_or(0)
    }
    /// `targets` holds, by call site, the plans the lease binds there.
    pub(super) fn bind(
        caller: &Code,
        targets: &[Vec<Arc<CalleePlan>>],
        codes: &[Option<Arc<Code>>],
    ) -> Result<Self, RegionError> {
        if targets.len() != caller.calls.len() {
            return Err(invalid(Refused::Source));
        }
        let sites = targets
            .iter()
            .map(|plans| {
                let mut targets = plans
                    .iter()
                    .map(|plan| {
                        let callee = codes
                            .get(plan.code().0 as usize)
                            .and_then(Option::as_ref)
                            .ok_or_else(|| invalid(Refused::Source))?;
                        if !Arc::ptr_eq(plan.source(), callee.program.source()) {
                            return Err(invalid(Refused::Source));
                        }
                        let parameters = plan
                            .substitutions()
                            .iter()
                            .map(|substitution| {
                                let binding = substitution.parameter();
                                let input = match substitution {
                                    Substitution::Role { argument, .. } => {
                                        Input::Register(role(&caller.program, argument.slot())?)
                                    }
                                    Substitution::Absent { .. } => Input::Absent,
                                    Substitution::Guard { value, .. } => Input::Guard(*value),
                                };
                                Ok(Parameter {
                                    input,
                                    output: role(&callee.program, binding.role.slot())?,
                                    shared: binding.shared,
                                })
                            })
                            .collect::<Result<_, RegionError>>()?;
                        let captures = plan
                            .captures()
                            .iter()
                            .map(|capture| {
                                Ok((capture.index, role(&callee.program, capture.role.slot())?))
                            })
                            .collect::<Result<_, RegionError>>()?;
                        Ok(Target {
                            code: plan.code(),
                            parameters,
                            captures,
                        })
                    })
                    .collect::<Result<Vec<_>, RegionError>>()?;
                targets.sort_unstable_by_key(|target| target.code);
                if targets.windows(2).any(|pair| pair[0].code == pair[1].code) {
                    return Err(invalid(Refused::Source));
                }
                Ok(targets.into_boxed_slice())
            })
            .collect::<Result<_, RegionError>>()?;
        Ok(Self { sites })
    }

    pub(super) fn target(&self, site: u32, code: CodeId) -> Result<&Target, RegionError> {
        let targets = self
            .sites
            .get(site as usize)
            .ok_or_else(|| invalid(Refused::Source))?;
        let index = targets
            .binary_search_by_key(&code, |target| target.code)
            .map_err(|_| invalid(Refused::Source))?;
        Ok(&targets[index])
    }
}
