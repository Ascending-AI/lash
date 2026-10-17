//! Pressure-only accounting. A certificate certifies the allocated census,
//! including garbage, rather than the cache or the last marked live set.

mod trace;

pub use trace::{TraceLimits, TraceStats};

use lash_kernel_doc::heap::LogicalBytes;

use super::census::{
    AccountingCertificate, AccountingMetric, AllocationCensus, CensusFault, CensusGraph,
    LogicalPermit, MIN_ENTITY_COST, PendingReservation, RefinedDecision, ReservationDecision,
    TransientRoots,
};

/// The admitted census ledger. No operation except `refine` traverses the
/// graph, and restoring a certificate does not reconstruct any objects.
pub struct Census {
    certificate: AccountingCertificate,
    /// The last seal or exact collection. Only growth above it is refundable.
    base: LogicalBytes,
    young: LogicalBytes,
    bound: LogicalBytes,
    limits: TraceLimits,
    last_trace: TraceStats,
}

impl Census {
    /// Host limits change the ability to finish a proof, never its verdict.
    pub fn set_trace_limits(&mut self, limits: TraceLimits) -> Result<(), CensusFault> {
        limits.validate()?;
        self.limits = limits;
        Ok(())
    }

    /// Full accounting validation for a foreign population whose individual
    /// summaries have already been checked against their semantic pages.
    /// Ordinary admitted restore never runs this walk.
    pub fn validate_population(
        certificate: AccountingCertificate,
        graph: &mut dyn CensusGraph,
        limits: TraceLimits,
    ) -> Result<(), CensusFault> {
        Self::validate(certificate)?;
        limits.validate()?;
        let (cost, entities) = trace::population_cost(graph, limits)?;
        if entities != certificate.entities || cost > certificate.upper {
            return Err(malformed(
                "certificate undercounts the validated population",
            ));
        }
        Ok(())
    }

    pub fn last_trace_stats(&self) -> TraceStats {
        self.last_trace
    }

    fn validate(certificate: AccountingCertificate) -> Result<(), CensusFault> {
        let minimum = certificate
            .entities
            .checked_mul(MIN_ENTITY_COST.0)
            .ok_or_else(|| malformed("entity minimum overflow"))?;
        if certificate.upper.0 < minimum {
            return Err(malformed("upper bound is below the census entity minimum"));
        }
        Ok(())
    }
}

impl AllocationCensus for Census {
    fn restore(
        certificate: AccountingCertificate,
        bound: LogicalBytes,
    ) -> Result<Self, CensusFault> {
        Self::validate(certificate)?;
        Ok(Self {
            certificate,
            base: certificate.upper,
            young: LogicalBytes(0),
            bound,
            limits: TraceLimits::default(),
            last_trace: TraceStats::default(),
        })
    }

    fn certificate(&self) -> AccountingCertificate {
        #[cfg(any(test, feature = "perf-witness"))]
        crate::perf_witness::record(|work| work.accounting_certificate_reads += 1);
        self.certificate
    }

    fn rebase(&mut self) {
        self.base = self.certificate.upper;
        self.young = LogicalBytes(0);
    }

    fn added(&mut self, cost: LogicalBytes, entities: u64) -> Result<(), CensusFault> {
        #[cfg(any(test, feature = "perf-witness"))]
        crate::perf_witness::record(|work| work.accounting_additions += 1);
        let minimum = entities
            .checked_mul(MIN_ENTITY_COST.0)
            .ok_or_else(|| malformed("entity minimum overflow"))?;
        if cost.0 < minimum {
            return Err(malformed("entity cost is below its minimum"));
        }
        let mut next = self.certificate;
        next.upper = LogicalBytes(
            next.upper
                .0
                .checked_add(cost.0)
                .ok_or_else(|| malformed("census addition overflow"))?,
        );
        next.entities = next
            .entities
            .checked_add(entities)
            .ok_or_else(|| malformed("entity count overflow"))?;
        Self::validate(next)?;
        self.young = LogicalBytes(self.young.0 + cost.0);
        self.certificate = next;
        Ok(())
    }

    fn changed(&mut self, before: LogicalBytes, after: LogicalBytes) -> Result<(), CensusFault> {
        #[cfg(any(test, feature = "perf-witness"))]
        crate::perf_witness::record(|work| work.accounting_changes += 1);
        let mut next = self.certificate;
        // An old object stays in the baseline until precision proves it
        // dead. Local decreases and release-by-count refund only the young
        // portion; transferring a hold into a projected row is a net delta.
        let young = self.young.0;
        let next_young = if after.0 >= before.0 {
            young.checked_add(after.0 - before.0)
        } else {
            Some(young.saturating_sub(before.0 - after.0))
        };
        next.upper = LogicalBytes(
            next_young
                .and_then(|young| self.base.0.checked_add(young))
                .ok_or_else(|| malformed("local cost change overflow"))?,
        );
        Self::validate(next)?;
        self.young = LogicalBytes(next.upper.0 - self.base.0);
        self.certificate = next;
        Ok(())
    }

    fn reserve(&mut self, bytes: LogicalBytes) -> ReservationDecision {
        #[cfg(any(test, feature = "perf-witness"))]
        crate::perf_witness::record(|work| work.accounting_reservations += 1);
        let total = self.certificate.upper.0.checked_add(bytes.0);
        if total.is_some_and(|total| total <= self.bound.0) {
            ReservationDecision::Granted(LogicalPermit { bytes })
        } else {
            ReservationDecision::NeedsPrecision(PendingReservation {
                bytes,
                overflowed: total.is_none(),
            })
        }
    }

    fn refine(
        &mut self,
        graph: &mut dyn CensusGraph,
        transient: &TransientRoots<'_>,
        pending: PendingReservation,
    ) -> Result<RefinedDecision, CensusFault> {
        #[cfg(any(test, feature = "perf-witness"))]
        crate::perf_witness::record(|work| work.refinement_attempts += 1);
        // All fallible marking finishes before any sweep. A failure never
        // installs a certificate or returns a partial guest decision.
        let refinements = self
            .certificate
            .refinements
            .checked_add(1)
            .ok_or_else(|| malformed("refinement count overflow"))?;
        let proof = trace::trace(graph, transient, self.limits)?;
        let next = AccountingCertificate {
            metric: AccountingMetric::LocalCostV1,
            upper: proof.reachable,
            entities: proof.entities,
            refinements,
        };
        Self::validate(next)?;
        self.last_trace = proof.sweep(graph)?;
        #[cfg(any(test, feature = "perf-witness"))]
        crate::perf_witness::record(|work| {
            work.completed_refinements += 1;
            work.completed_trace_work += self.last_trace.work;
            work.trace_bitmap_peak_bytes = work
                .trace_bitmap_peak_bytes
                .max(self.last_trace.bitmap_peak as u64);
            work.trace_work_list_peak_bytes = work
                .trace_work_list_peak_bytes
                .max(self.last_trace.work_list_peak as u64);
            work.trace_budget_peak_bytes = work
                .trace_budget_peak_bytes
                .max(self.last_trace.budget_peak as u64);
        });
        self.certificate = next;
        self.base = next.upper;
        self.young = LogicalBytes(0);
        Ok(match next.upper.0.checked_add(pending.bytes.0) {
            Some(total) if total <= self.bound.0 => RefinedDecision::Granted(LogicalPermit {
                bytes: pending.bytes,
            }),
            _ => RefinedDecision::Refused {
                reachable: next.upper,
            },
        })
    }
}

fn malformed(reason: &str) -> CensusFault {
    CensusFault::Malformed(reason.to_owned())
}
