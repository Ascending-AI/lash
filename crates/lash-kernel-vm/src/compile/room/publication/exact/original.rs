//! Original charge payment travels through the already-authenticated boundary.
use super::*;
use crate::compile::room::charges::{AcceptedOriginalPrefix, OriginalPayment, OriginalPrefix};

pub(crate) struct OriginalBound {
    bound: BoundEvent,
    payment: OriginalPayment,
    roles: Box<[(u32, logical::PublicationValue)]>,
}
pub(crate) struct OriginalPrepared<'a> {
    prepared: PreparedEvent<'a>,
    payment: OriginalPayment,
    roles: Box<[(u32, logical::PublicationValue)]>,
}

impl BoundEvent {
    #[expect(
        clippy::too_many_arguments,
        reason = "one original source prefix and its actual roles"
    )]
    pub(crate) fn with_original_prefix(
        mut self,
        frontier: &Frontier,
        ledger: &Ledger,
        arena: &Arena,
        heap: &PagedHeap,
        program: &Arc<OriginalPrefix>,
        registers: &[doc::Value],
        previous: Option<&AcceptedOriginalPrefix>,
    ) -> Result<OriginalBound, PublicationError> {
        if !current(&self.frontier, self.revision, &self.before, frontier) {
            return Err(PublicationError::Miss("original prefix frontier changed"));
        }
        let payment = program
            .payment(frontier, ledger, &self.allowance, previous)
            .map_err(|_| PublicationError::Miss("original prefix charge mismatch"))?;
        let roles = program
            .snapshot(frontier, ledger, heap, registers, &self.roots.outputs)
            .map_err(|_| PublicationError::Miss("original prefix roles changed"))?;
        let handles: Vec<_> = roles
            .iter()
            .filter_map(|(_, value)| match value {
                logical::PublicationValue::Native(value) => value.handle(),
                _ => None,
            })
            .collect();
        let dependencies = Dependencies::capture(ledger, arena, &handles).ok_or(
            PublicationError::Miss("original prefix dependencies unavailable"),
        )?;
        self.dependencies.extend(dependencies)?;
        self.allowance = payment.need.clone();
        Ok(OriginalBound {
            bound: self,
            payment,
            roles,
        })
    }

    pub(crate) fn with_original_native(
        mut self,
        frontier: &Frontier,
        ledger: &Ledger,
        prefix: &AcceptedOriginalPrefix,
        debt: &crate::compile::room::charges::ChargeDebt<'_>,
    ) -> Result<OriginalBound, PublicationError> {
        if !current(&self.frontier, self.revision, &self.before, frontier) {
            return Err(PublicationError::Miss("original native frontier changed"));
        }
        let payment = prefix
            .native_payment(frontier, ledger, debt, self.allowance.safe_point)
            .map_err(|_| PublicationError::Miss("original native charge mismatch"))?;
        if debt.native_allowance(self.allowance.safe_point).as_ref() != Some(&self.allowance) {
            return Err(PublicationError::Miss("original native allowance changed"));
        }
        self.allowance = payment.need.clone();
        Ok(OriginalBound {
            bound: self,
            payment,
            roles: Box::default(),
        })
    }
}

impl OriginalBound {
    pub(crate) fn prepare<'a>(
        self,
        frontier: &Frontier,
        ledger: &Ledger,
        registration: Registration<'a>,
        heap: &PagedHeap,
    ) -> Result<OriginalPrepared<'a>, PublicationError> {
        Ok(OriginalPrepared {
            prepared: self.bound.prepare(frontier, ledger, registration, heap)?,
            payment: self.payment,
            roles: self.roles,
        })
    }
    pub(crate) fn finish_external<'a>(
        self,
        frontier: &Frontier,
        ledger: &Ledger,
        transaction: BoundaryTransaction<'a>,
    ) -> Result<OriginalPrepared<'a>, PublicationError> {
        Ok(OriginalPrepared {
            prepared: self.bound.finish_external(frontier, ledger, transaction)?,
            payment: self.payment,
            roles: self.roles,
        })
    }
}
impl<'a> OriginalPrepared<'a> {
    pub(crate) fn peak(&self) -> u64 {
        self.prepared.peak()
    }
    pub(crate) fn collect(mut self, ledger: &Ledger, heap: &PagedHeap) -> Result<Self, HeapFault> {
        self.prepared = self.prepared.collect(ledger, heap)?;
        Ok(self)
    }
    pub(crate) fn admit<'r>(
        self,
        frontier: &Frontier,
        ledger: &Ledger,
        resources: SourceResources<'r>,
        bound: u64,
    ) -> Result<Option<SourceAccepted<'r, (Self, u64)>>, HeapFault> {
        let payment = self.payment;
        let roles = self.roles;
        self.prepared
            .admit(frontier, ledger, resources, bound)?
            .map(|accepted| {
                accepted.try_map(|(prepared, bound)| {
                    Ok((
                        Self {
                            prepared,
                            payment,
                            roles,
                        },
                        bound,
                    ))
                })
            })
            .transpose()
    }
}
impl SourceAccepted<'_, (OriginalPrepared<'_>, u64)> {
    /// The owner's receipt changes only after successful publication. A stale,
    /// abandoned or declined suffix keeps the same already-paid prefix.
    pub(crate) fn publish_original_event(
        self,
        frontier: &mut Frontier,
        ledger: &mut Ledger,
        previous: &mut Option<AcceptedOriginalPrefix>,
        semantic: impl FnOnce(&mut Arena, Vec<MemberEdit>),
        tape: Option<&crate::observe::ChargeTape>,
    ) -> Result<logical::Balance, PublicationError> {
        let mut payment = None;
        let mut roles = None;
        let accepted = self.try_map(|(prepared, bound)| {
            if !prepared.payment.validate_prior(previous.as_ref())
                || !prepared.payment.valid_roles(&prepared.roles)
            {
                return Err(PublicationError::Miss("original receipt changed"));
            }
            payment = Some(prepared.payment);
            roles = Some(prepared.roles);
            Ok((prepared.prepared, bound))
        })?;
        let balance = accepted.publish_event(frontier, ledger, semantic)?;
        let payment =
            payment.unwrap_or_else(|| unreachable!("original payment retained during commit"));
        *previous = payment.published(
            frontier,
            ledger,
            previous.take(),
            roles.unwrap_or_else(|| unreachable!("original roles retained during commit")),
            tape,
        );
        Ok(balance)
    }
}
