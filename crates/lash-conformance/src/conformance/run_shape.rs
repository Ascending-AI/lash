//! Shared run-shape counting machinery for the property harnesses.
//!
//! Each harness declares one fieldless enum implementing [`Counter`];
//! [`RunShape`], [`RunShapeTotals`], and the report all derive from
//! `Counter::ALL`, so a new counter cannot be counted without being gated and
//! reported.

use std::fmt;
use std::marker::PhantomData;
use std::sync::atomic::{AtomicU64, Ordering};

/// One harness's run-shape counter alphabet.
pub(crate) trait Counter: Copy + fmt::Debug + 'static {
    /// Every counter, in report order.
    const ALL: &'static [Self];
    /// The counter's label in the report and in starvation messages.
    fn name(self) -> &'static str;
    /// The counter's position in `ALL`.
    fn index(self) -> usize;
}

/// Per-case counter values, indexed by the harness's [`Counter`] enum.
pub(crate) struct RunShape<C: Counter> {
    counts: Box<[u64]>,
    marker: PhantomData<C>,
}

impl<C: Counter> Default for RunShape<C> {
    fn default() -> Self {
        assert!(
            C::ALL
                .iter()
                .enumerate()
                .all(|(position, counter)| counter.index() == position),
            "Counter::ALL must list every variant in declaration order"
        );
        Self {
            counts: vec![0; C::ALL.len()].into(),
            marker: PhantomData,
        }
    }
}

impl<C: Counter> Clone for RunShape<C> {
    fn clone(&self) -> Self {
        Self {
            counts: self.counts.clone(),
            marker: PhantomData,
        }
    }
}

impl<C: Counter> fmt::Debug for RunShape<C> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut map = f.debug_map();
        for counter in C::ALL {
            map.entry(&counter.name(), &self.counts[counter.index()]);
        }
        map.finish()
    }
}

impl<C: Counter> std::ops::Index<C> for RunShape<C> {
    type Output = u64;
    fn index(&self, counter: C) -> &u64 {
        &self.counts[counter.index()]
    }
}

impl<C: Counter> std::ops::IndexMut<C> for RunShape<C> {
    fn index_mut(&mut self, counter: C) -> &mut u64 {
        &mut self.counts[counter.index()]
    }
}

/// Cross-case aggregates over the same counter alphabet.
pub(crate) struct RunShapeTotals<C: Counter> {
    counts: Box<[AtomicU64]>,
    marker: PhantomData<C>,
}

impl<C: Counter> Default for RunShapeTotals<C> {
    fn default() -> Self {
        Self {
            counts: (0..C::ALL.len()).map(|_| AtomicU64::new(0)).collect(),
            marker: PhantomData,
        }
    }
}

impl<C: Counter> fmt::Debug for RunShapeTotals<C> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RunShapeTotals")
            .field("report", &self.report())
            .finish()
    }
}

impl<C: Counter> RunShapeTotals<C> {
    pub(crate) fn add(&self, shape: &RunShape<C>) {
        for counter in C::ALL {
            self.counts[counter.index()]
                .fetch_add(shape.counts[counter.index()], Ordering::Relaxed);
        }
    }

    pub(crate) fn report(&self) -> String {
        C::ALL
            .iter()
            .map(|counter| {
                format!(
                    "{}={}",
                    counter.name(),
                    self.counts[counter.index()].load(Ordering::Relaxed)
                )
            })
            .collect::<Vec<_>>()
            .join(" ")
    }
}
