//! A paid original prefix is an affine receipt, never a new price authority.
use super::*;
use crate::compile::room::publication::{Frontier, Occurrence};
use lash_kernel_exec::charge::RegionEvent;
use lash_kernel_state::kernel_internals::logical::{Ledger, PublicationValue};
use std::rc::Rc;

struct Region {
    close: expr::Close,
    events: Box<[RegionEvent]>,
    at: Box<[u32]>,
    paid: usize,
}

/// Cached with the constructor site, independently of physical runtime state.
pub(crate) struct OriginalPrefix {
    source: Arc<Source>,
    code: compile::CodeId,
    executable: u64,
    statement: compile::StmtId,
    at: Point,
    next: u32,
    stop: u32,
    end: Option<Point>,
    regions: Box<[Region]>,
    roles: Box<[(u32, crate::compile::room::Owner)]>,
    registers: usize,
    recording: bool,
    paid_units: u64,
    total_units: u64,
    paid_tape: Box<[Weight]>,
    remaining_tape: Box<[Weight]>,
}

/// Only an accepted exact publication issues this receipt. It cannot be cloned
/// or serialized. Saved operands use source roles and exact value provenance.
pub(crate) struct AcceptedOriginalPrefix {
    program: Arc<OriginalPrefix>,
    ledger: lash_kernel_state::kernel_internals::logical::Stamp,
    identity: Rc<()>,
    frontier: Rc<()>,
    occurrence: Occurrence,
    paid_at: u64,
    roles: Box<[(u32, PublicationValue)]>,
}

pub(in crate::compile::room) struct OriginalPayment {
    pub(in crate::compile::room) program: Arc<OriginalPrefix>,
    pub(in crate::compile::room) prior: Option<Rc<()>>,
    pub(in crate::compile::room) need: crate::admission::Allowance,
    tape: PaymentTape,
    pub(in crate::compile::room) continuing: bool,
}

enum PaymentTape {
    Prefix { previous: usize },
    Native { formulas: Box<[u64]> },
    Return { formula: u64 },
}

pub(crate) struct OriginalReplay {
    pub(crate) next: u32,
    pub(crate) stop: u32,
    pub(crate) events: Box<[(u32, RegionEvent)]>,
    pub(crate) roles: Box<[(u32, Site, PublicationValue)]>,
}

impl OriginalPrefix {
    pub(crate) fn compile(
        exe: &Executable,
        code: compile::CodeId,
        statement: compile::StmtId,
        prefix: &expr::ConstructorPrefix,
    ) -> Result<Arc<Self>, Refused> {
        ChargeProgram::constructor_packet(exe, code, statement, prefix)?.verify(exe, code)?;
        let (stream, stmt, _) = exe.statement(statement);
        let pc = prefix.pc();
        let at = source_point(
            stream,
            stream.source_sites[pc as usize]
                .as_ref()
                .ok_or(Refused::Source)?,
            stream.ops[pc as usize],
        );
        let next = pc.checked_add(1).ok_or(Refused::Path)?;
        let stop = (next..stmt.ops.end())
            .find(|pc| {
                let op = stream.ops[*pc as usize];
                matches!(op, Op::Region { .. }) || !op.kind().in_region()
            })
            .ok_or(Refused::Path)?;
        // A layout region head is a legal source handoff, but is not a
        // native/write/return completion whose debt can consume this receipt.
        let end = stream.source_sites[stop as usize]
            .as_deref()
            .map(|site| source_point(stream, site, stream.ops[stop as usize]));
        if end.is_none() && !matches!(stream.ops[stop as usize], Op::Region { .. }) {
            return Err(Refused::Source);
        }
        let mut regions = Vec::new();
        for close in prefix.pending() {
            let events = stream.regions.events(close.region);
            let positions = stream.regions.at(close.region);
            if positions.iter().any(|at| *at > stop) {
                return Err(Refused::Source);
            }
            regions.push(Region {
                close: *close,
                events: events.into(),
                at: positions.into(),
                paid: stream.regions.executed(close.region, pc),
            });
        }
        let mut unpaid = false;
        for region in &regions {
            if unpaid && region.paid != 0 {
                return Err(Refused::Source);
            }
            unpaid |= region.paid < region.events.len();
        }
        let weights = |paid: bool| {
            regions
                .iter()
                .flat_map(move |region| {
                    let events = if paid {
                        &region.events[..region.paid]
                    } else {
                        &region.events[region.paid..]
                    };
                    events.iter().map(|event| event.weight)
                })
                .collect::<Vec<_>>()
        };
        let (paid_tape, remaining_tape) = if exe.code(code).charged {
            (weights(true), weights(false))
        } else {
            (vec![], vec![])
        };
        let paid_units = paid_tape
            .iter()
            .try_fold(0_u64, |sum, weight| sum.checked_add(weight.units()))
            .ok_or(Refused::Facts)?;
        let total_units = remaining_tape
            .iter()
            .try_fold(paid_units, |sum, weight| sum.checked_add(weight.units()))
            .ok_or(Refused::Facts)?;
        let roles = stmt
            .source_roles_at(stream, next)
            .ok_or(Refused::Source)?
            .iter()
            .map(|(reg, site)| {
                (
                    reg.0,
                    crate::compile::room::Owner::Operand((**site).clone()),
                )
            })
            .collect();
        Ok(Arc::new(Self {
            source: Arc::clone(&exe.code(code).source_room),
            code,
            executable: exe.local(),
            statement,
            at,
            next,
            stop,
            end,
            regions: regions.into(),
            roles,
            registers: stmt.regs as usize,
            recording: exe.charging(),
            paid_units,
            total_units,
            paid_tape: paid_tape.into(),
            remaining_tape: remaining_tape.into(),
        }))
    }

    pub(crate) fn values(&self, mut read: impl FnMut(u32) -> Value) -> Vec<Value> {
        let mut values = vec![Value::Absent; self.registers];
        for (reg, role) in &self.roles {
            if !matches!(role, crate::compile::room::Owner::Operand(site) if site == &self.at.site)
            {
                values[*reg as usize] = read(*reg);
            }
        }
        values
    }
    pub(crate) fn pc(&self) -> u32 {
        self.next - 1
    }
    pub(crate) fn unpaid(&self) -> u32 {
        self.regions
            .first()
            .map_or(self.pc(), |region| region.close.head)
    }
    pub(crate) fn matches_constructor(
        &self,
        stream: &compile::Stream,
        pc: u32,
        unpaid: u32,
        quote: u64,
    ) -> bool {
        self.pc() == pc
            && self.unpaid() == unpaid
            && self
                .regions
                .iter()
                .map(|region| u64::from(region.close.total))
                .sum::<u64>()
                == quote
            && stream
                .source_sites
                .get(pc as usize)
                .and_then(Option::as_deref)
                .is_some_and(|site| self.at == source_point(stream, site, stream.ops[pc as usize]))
            && self
                .regions
                .iter()
                .all(|region| compile::islands::verify_close(Some(&region.close), stream).is_ok())
    }

    fn compatible(&self, previous: &Self) -> bool {
        Arc::ptr_eq(&self.source, &previous.source)
            && self.executable == previous.executable
            && self.code == previous.code
            && self.statement == previous.statement
            && self.next >= previous.next
            && self.stop == previous.stop
            && self.regions.len() == previous.regions.len()
            && self
                .regions
                .iter()
                .zip(&previous.regions)
                .all(|(next, old)| next.close == old.close && next.paid >= old.paid)
    }

    pub(in crate::compile::room) fn snapshot(
        &self,
        frontier: &Frontier,
        ledger: &Ledger,
        heap: &lash_kernel_state::kernel_internals::PagedHeap,
        registers: &[Value],
        outputs: &[(
            lash_kernel_state::kernel_internals::logical::Owner,
            PublicationValue,
        )],
    ) -> Result<Box<[(u32, PublicationValue)]>, Refused> {
        if !Arc::ptr_eq(&self.source, &frontier.source)
            || self.at != frontier.position.at
            || registers.len() != self.registers
            || frontier.proven
        {
            return Err(Refused::Source);
        }
        let slots = self.source.owner_slots()?;
        self.roles
            .iter()
            .map(|(reg, selector)| {
                let owner = slots
                    .root(
                        selector,
                        frontier.position.occurrence.task,
                        frontier.position.occurrence.invocation,
                    )
                    .ok_or(Refused::Source)?;
                let value =
                    if let Some((_, value)) = outputs.iter().find(|(root, _)| *root == owner) {
                        value.clone()
                    } else {
                        // Physical values are borrowed through the compiler's exact
                        // layout; a caller cannot supply another role/register map.
                        if ledger
                            .holding(owner, heap.pages())
                            .map_err(|_| Refused::Facts)?
                            .is_none()
                        {
                            return Err(Refused::Retained);
                        }
                        PublicationValue::Native(registers[*reg as usize])
                    };
                Ok((*reg, value))
            })
            .collect()
    }

    pub(in crate::compile::room) fn payment(
        self: &Arc<Self>,
        frontier: &Frontier,
        ledger: &Ledger,
        original: &crate::admission::Allowance,
        previous: Option<&AcceptedOriginalPrefix>,
    ) -> Result<OriginalPayment, Refused> {
        if !Arc::ptr_eq(&self.source, &frontier.source)
            || self.at != frontier.position.at
            || original.units != 0
            || original.before != 0
            || original.close.is_some()
            || original.depth != 0
            || !original.occurrences.is_empty()
        {
            return Err(Refused::Source);
        }
        if let Some(previous) = previous
            && (!previous.belongs(frontier, ledger)
                || !self.compatible(&previous.program)
                || previous.paid_at != original.safe_point)
        {
            return Err(Refused::Source);
        }
        let units = self
            .paid_units
            .checked_sub(previous.map_or(0, |old| old.program.paid_units))
            .ok_or(Refused::Facts)?;
        Ok(OriginalPayment {
            program: Arc::clone(self),
            prior: previous.map(|old| Rc::clone(&old.identity)),
            need: crate::admission::Allowance {
                units,
                strict: true,
                ..original.clone()
            },
            tape: PaymentTape::Prefix {
                previous: previous.map_or(0, |old| old.program.paid_tape.len()),
            },
            continuing: true,
        })
    }
}

impl AcceptedOriginalPrefix {
    fn valid_recovery(&self) -> bool {
        self.program.roles.len() == self.roles.len()
            && self
                .program
                .roles
                .iter()
                .zip(&self.roles)
                .all(|((expected, _), (actual, _))| expected == actual)
    }

    fn belongs(&self, frontier: &Frontier, ledger: &Ledger) -> bool {
        ledger.owns_stamp(&self.ledger)
            && Rc::ptr_eq(&self.frontier, &frontier.identity)
            && Arc::ptr_eq(&self.program.source, &frontier.source)
            && self.occurrence == frontier.position.occurrence
            && frontier.position.unpaid.is_some()
            && self.valid_recovery()
    }

    pub(in crate::compile::room) fn native_payment(
        &self,
        frontier: &Frontier,
        ledger: &Ledger,
        debt: &ChargeDebt<'_>,
        safe_point: u64,
    ) -> Result<OriginalPayment, Refused> {
        if !self.belongs(frontier, ledger)
            || safe_point != self.paid_at
            || !Arc::ptr_eq(&self.program.source, &debt.program.source)
            || self.program.statement != debt.program.statement
            || self.program.end.as_ref() != Some(&debt.program.first)
            || debt.program.closes.len() != 1
            || debt.program.closes[0].regions.as_ref()
                != self
                    .program
                    .regions
                    .iter()
                    .map(|r| r.close)
                    .collect::<Vec<_>>()
        {
            return Err(Refused::Source);
        }
        let original = debt.native_allowance(safe_point).ok_or(Refused::Source)?;
        if debt.program.fixed != self.program.total_units {
            return Err(Refused::Source);
        }
        let units = original
            .units
            .checked_sub(self.program.paid_units)
            .ok_or(Refused::Facts)?;
        Ok(OriginalPayment {
            program: Arc::clone(&self.program),
            prior: Some(Rc::clone(&self.identity)),
            need: crate::admission::Allowance {
                units,
                ..original.clone()
            },
            tape: PaymentTape::Native {
                formulas: debt.formulas.clone().into(),
            },
            continuing: false,
        })
    }

    pub(in crate::compile::room) fn return_payment(
        &self,
        frontier: &Frontier,
        ledger: &Ledger,
        original: &crate::admission::Allowance,
    ) -> Result<OriginalPayment, Refused> {
        if !self.belongs(frontier, ledger)
            || original.safe_point != self.paid_at
            || self.program.end.as_ref() != Some(&frontier.position.at)
            || original.before != 0
            || original.close.is_some()
            || original.depth != 0
            || original.strict
            || !original.occurrences.is_empty()
        {
            return Err(Refused::Source);
        }
        let fixed = self
            .program
            .total_units
            .checked_sub(self.program.paid_units)
            .ok_or(Refused::Facts)?;
        Ok(OriginalPayment {
            program: Arc::clone(&self.program),
            prior: Some(Rc::clone(&self.identity)),
            need: crate::admission::Allowance {
                units: original.units.checked_add(fixed).ok_or(Refused::Facts)?,
                strict: true,
                ..original.clone()
            },
            tape: PaymentTape::Return {
                formula: original.units,
            },
            continuing: false,
        })
    }

    #[cfg(test)]
    pub(crate) fn corrupt_recovery_for_law(&mut self, missing: bool) {
        if missing {
            self.roles = Box::default();
        } else {
            self.roles[0].0 = self.roles[0].0.wrapping_add(1);
        }
    }

    #[expect(clippy::too_many_arguments, reason = "one actual source continuation")]
    pub(crate) fn into_replay(
        self,
        source: &Arc<Source>,
        ledger: &Ledger,
        code: compile::CodeId,
        statement: compile::StmtId,
        task: doc::TaskId,
        invocation: u64,
        charged: u64,
    ) -> Result<OriginalReplay, Refused> {
        if !ledger.owns_stamp(&self.ledger)
            || !Arc::ptr_eq(source, &self.program.source)
            || code != self.program.code
            || statement != self.program.statement
            || task != self.occurrence.task
            || invocation != self.occurrence.invocation
            || charged != self.paid_at
            || !self.valid_recovery()
        {
            return Err(Refused::Source);
        }
        let events = self
            .program
            .regions
            .iter()
            .flat_map(|region| {
                region.at[region.paid..]
                    .iter()
                    .copied()
                    .zip(region.events[region.paid..].iter().copied())
            })
            .collect::<Vec<_>>();
        if events.windows(2).any(|pair| pair[0].0 > pair[1].0)
            || events
                .iter()
                .any(|(at, _)| *at < self.program.next || *at > self.program.stop)
        {
            return Err(Refused::Source);
        }
        Ok(OriginalReplay {
            next: self.program.next,
            stop: self.program.stop,
            events: events.into(),
            roles: self
                .roles
                .into_vec()
                .into_iter()
                .zip(self.program.roles.iter())
                .map(|((reg, value), (_, selector))| {
                    let crate::compile::room::Owner::Operand(site) = selector else {
                        unreachable!("compiled operand role")
                    };
                    (reg, site.clone(), value)
                })
                .collect(),
        })
    }
}

impl OriginalPayment {
    pub(in crate::compile::room) fn valid_roles(&self, roles: &[(u32, PublicationValue)]) -> bool {
        if !self.continuing {
            return roles.is_empty();
        }
        self.program.roles.len() == roles.len()
            && self
                .program
                .roles
                .iter()
                .zip(roles)
                .all(|((expected, _), (actual, _))| expected == actual)
    }
    pub(in crate::compile::room) fn validate_prior(
        &self,
        prior: Option<&AcceptedOriginalPrefix>,
    ) -> bool {
        match (&self.prior, prior) {
            (None, None) => true,
            (Some(identity), Some(prior)) => Rc::ptr_eq(identity, &prior.identity),
            _ => false,
        }
    }
    pub(in crate::compile::room) fn published(
        self,
        frontier: &Frontier,
        ledger: &Ledger,
        prior: Option<AcceptedOriginalPrefix>,
        roles: Box<[(u32, PublicationValue)]>,
        tape: Option<&crate::observe::ChargeTape>,
    ) -> Option<AcceptedOriginalPrefix> {
        assert!(
            self.validate_prior(prior.as_ref()),
            "original prefix receipt changed"
        );
        if self.program.recording
            && let Some(tape) = tape
        {
            match &self.tape {
                PaymentTape::Prefix { previous } => {
                    for weight in &self.program.paid_tape[*previous..] {
                        tape.push(*weight);
                    }
                }
                PaymentTape::Native { formulas } => {
                    for weight in &self.program.remaining_tape {
                        tape.push(*weight);
                    }
                    for units in formulas.iter().copied().filter(|units| *units != 0) {
                        tape.push(Weight::Atomic(units));
                    }
                }
                PaymentTape::Return { formula } => {
                    for weight in &self.program.remaining_tape {
                        tape.push(*weight);
                    }
                    if *formula != 0 {
                        tape.push(Weight::Atomic(*formula));
                    }
                }
            }
        }
        self.continuing.then(|| AcceptedOriginalPrefix {
            program: self.program,
            ledger: ledger.stamp(),
            identity: Rc::new(()),
            frontier: Rc::clone(&frontier.identity),
            occurrence: frontier.position.occurrence.clone(),
            paid_at: self
                .need
                .safe_point
                .checked_add(self.need.units)
                .unwrap_or_else(|| unreachable!("admitted original charge")),
            roles,
        })
    }
}
