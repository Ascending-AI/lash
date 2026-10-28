//! The allocation census. A certificate counts the allocated census,
//! including garbage, rather than the cache or the last marked live set.

mod trace;

pub use trace::{TraceLimits, TraceStats};

use super::census::{
    AccountingCertificate, AccountingMetric, AllocationCensus, CensusFault, CensusGraph,
    TransientRoots,
};

/// The admitted census. No operation except `refine` traverses the graph,
/// and restoring a certificate does not reconstruct any objects.
pub struct Census {
    certificate: AccountingCertificate,
    limits: TraceLimits,
    last_trace: TraceStats,
    normalized: super::census::NormalizedHoldings,
}

impl Census {
    /// Host limits change the ability to finish a sweep, never its result.
    pub fn set_trace_limits(&mut self, limits: TraceLimits) -> Result<(), CensusFault> {
        limits.validate()?;
        self.limits = limits;
        Ok(())
    }

    /// Full membership validation for a foreign population whose
    /// individual summaries have already been checked against their
    /// semantic pages. Ordinary admitted restore never runs this walk.
    pub fn validate_population(
        certificate: AccountingCertificate,
        graph: &mut dyn CensusGraph,
        limits: TraceLimits,
    ) -> Result<(), CensusFault> {
        limits.validate()?;
        if trace::population(graph, limits)? != certificate.entities {
            return Err(malformed("certificate miscounts the validated population"));
        }
        Ok(())
    }

    /// Components from the last completed trace. Mutations do not refresh this
    /// receipt; it is not an incremental retained-balance ledger.
    pub fn normalized_holdings(&self) -> super::census::NormalizedHoldings {
        self.normalized
    }

    pub fn last_trace_stats(&self) -> TraceStats {
        self.last_trace
    }
}

impl AllocationCensus for Census {
    /// A heap written unmetered restores too: its first sweep counts its
    /// entities and meters it.
    fn restore(certificate: AccountingCertificate) -> Result<Self, CensusFault> {
        Ok(Self {
            certificate,
            limits: TraceLimits::default(),
            last_trace: TraceStats::default(),
            normalized: Default::default(),
        })
    }

    fn certificate(&self) -> AccountingCertificate {
        self.certificate
    }

    fn added(&mut self, entities: u64) -> Result<(), CensusFault> {
        #[cfg(any(test, feature = "perf-witness"))]
        crate::perf_witness::record(|work| work.accounting_additions += 1);
        self.certificate.entities = self
            .certificate
            .entities
            .checked_add(entities)
            .ok_or_else(|| malformed("entity count overflow"))?;
        Ok(())
    }

    fn record_capacity(&mut self, capacity: u64) {
        self.certificate.metric = AccountingMetric::Capacity;
        self.certificate.capacity = capacity;
    }

    fn refine(
        &mut self,
        graph: &mut dyn CensusGraph,
        transient: &TransientRoots<'_>,
    ) -> Result<(), CensusFault> {
        #[cfg(any(test, feature = "perf-witness"))]
        crate::perf_witness::record(|work| work.refinement_attempts += 1);
        // All fallible marking finishes before any sweep. A failure never
        // installs a certificate.
        let refinements = self
            .certificate
            .refinements
            .checked_add(1)
            .ok_or_else(|| malformed("refinement count overflow"))?;
        let (entities, holdings, stats) = normalized_trace(graph, transient, self.limits)?;
        self.last_trace = stats;
        self.normalized = holdings;
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
        self.certificate.metric = AccountingMetric::Capacity;
        self.certificate.entities = entities;
        self.certificate.refinements = refinements;
        Ok(())
    }
}

fn malformed(reason: &str) -> CensusFault {
    CensusFault::Malformed(reason.to_owned())
}

pub(crate) fn normalized_trace(
    graph: &mut dyn CensusGraph,
    transient: &TransientRoots<'_>,
    limits: TraceLimits,
) -> Result<(u64, super::census::NormalizedHoldings, TraceStats), CensusFault> {
    let proof = trace::trace(graph, transient, limits)?;
    let entities = proof.entities;
    let holdings = proof.holdings;
    let stats = proof.sweep(graph)?;
    Ok((entities, holdings, stats))
}
