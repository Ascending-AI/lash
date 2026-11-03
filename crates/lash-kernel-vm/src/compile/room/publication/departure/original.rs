//! A return adds only the unpaid original fixed suffix to its checked formula.
use super::*;
use crate::compile::room::charges::{AcceptedOriginalPrefix, OriginalPayment};
pub(crate) struct OriginalDeparture<'a> {
    prepared: PreparedDeparture<'a>,
    payment: OriginalPayment,
}
impl<'a> PreparedDeparture<'a> {
    pub(crate) fn with_original_return(
        mut self,
        frontier: &Frontier,
        ledger: &Ledger,
        prefix: &AcceptedOriginalPrefix,
    ) -> Result<OriginalDeparture<'a>, HeapFault> {
        if !self.current(frontier) {
            return Err(fault("original return frontier changed"));
        }
        let payment = prefix
            .return_payment(frontier, ledger, &self.allowance)
            .map_err(|_| fault("original return prefix changed"))?;
        self.allowance = payment.need.clone();
        Ok(OriginalDeparture {
            prepared: self,
            payment,
        })
    }
}
impl<'a> OriginalDeparture<'a> {
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
        self.prepared
            .admit(frontier, ledger, resources, bound)?
            .map(|accepted| {
                accepted.try_map(|(prepared, bound)| Ok((Self { prepared, payment }, bound)))
            })
            .transpose()
    }
}
impl SourceAccepted<'_, (OriginalDeparture<'_>, u64)> {
    pub(crate) fn publish_original_departure(
        self,
        frontier: &mut Frontier,
        ledger: &mut Ledger,
        prefix: &mut Option<AcceptedOriginalPrefix>,
        semantic: impl FnOnce(&mut Arena),
        tape: Option<&crate::observe::ChargeTape>,
    ) -> Result<logical::Balance, HeapFault> {
        let mut payment = None;
        let accepted = self.try_map(|(prepared, bound)| {
            if !prepared.payment.validate_prior(prefix.as_ref()) {
                return Err(fault("original return receipt changed"));
            }
            payment = Some(prepared.payment);
            Ok((prepared.prepared, bound))
        })?;
        let balance = accepted.publish_departure(frontier, ledger, semantic)?;
        let payment = payment
            .unwrap_or_else(|| unreachable!("original return payment retained during commit"));
        assert!(
            payment
                .published(frontier, ledger, prefix.take(), Box::default(), tape)
                .is_none()
        );
        Ok(balance)
    }
}
