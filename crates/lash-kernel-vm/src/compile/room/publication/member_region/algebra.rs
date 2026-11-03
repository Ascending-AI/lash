//! Reduce original source lifetimes to indexed price polynomials at compile
//! time. No source event evaluator or owner map runs inside a private kernel.
use super::*;
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct Polynomial {
    fixed: i128,
    terms: BTreeMap<usize, i128>,
}
impl Polynomial {
    fn term(&mut self, index: usize, coefficient: i128) -> Result<(), Refused> {
        let value = self
            .terms
            .get(&index)
            .copied()
            .unwrap_or(0)
            .checked_add(coefficient)
            .ok_or(Refused::Peak)?;
        if value == 0 {
            self.terms.remove(&index);
        } else {
            self.terms.insert(index, value);
        }
        Ok(())
    }
    fn add(&mut self, other: &Self, coefficient: i128) -> Result<(), Refused> {
        self.fixed = self
            .fixed
            .checked_add(other.fixed.checked_mul(coefficient).ok_or(Refused::Peak)?)
            .ok_or(Refused::Peak)?;
        for (&index, &weight) in &other.terms {
            self.term(index, weight.checked_mul(coefficient).ok_or(Refused::Peak)?)?;
        }
        Ok(())
    }
    pub(super) fn evaluate(&self, facts: &[i128]) -> Option<i128> {
        self.terms
            .iter()
            .try_fold(self.fixed, |total, (&index, &coefficient)| {
                total.checked_add(facts.get(index)?.checked_mul(coefficient)?)
            })
    }
}
fn amount(
    exe: &Executable,
    nodes: &[Node],
    index: usize,
    amount: &Amount,
) -> Result<Polynomial, Refused> {
    let node = &nodes[index];
    let mut polynomial = Polynomial {
        fixed: amount.fixed,
        terms: BTreeMap::new(),
    };
    let slot = |owner: &Owner| {
        if node.event.output.as_ref() == Some(owner) {
            return Ok(3 * index);
        }
        node.event
            .inputs
            .iter()
            .position(|input| input == owner)
            .map(|at| 3 * node.inputs[at])
            .ok_or(Refused::Retained)
    };
    for (measure, coefficient) in &amount.terms {
        match measure {
            Measure::Local(owner) => polynomial.term(slot(owner)?, *coefficient)?,
            // Bodies present at entry remain in the interior envelope. Only
            // direct occurrences are released here; canonical graph release
            // happens in the final transaction. This is an upper bound even
            // when an earlier source event would have reclaimed a child.
            Measure::Release(owners) | Measure::NativeRelease { owners, .. } => {
                for owner in owners {
                    polynomial.term(slot(owner)?, *coefficient)?;
                }
            }
            Measure::MemberDelta(at) | Measure::MemberGrowth(at)
                if *at == node.event.point.site
                    && matches!(node.operation, Operation::Write { .. }) =>
            {
                polynomial.term(
                    3 * index
                        + if matches!(measure, Measure::MemberGrowth(_)) {
                            2
                        } else {
                            1
                        },
                    *coefficient,
                )?;
            }
            Measure::CallPeak { at, callee } | Measure::CallDelta { at, callee } => {
                let Operation::Native { lib, .. } = node.operation else {
                    return Err(Refused::Source);
                };
                if at != &node.event.point.site
                    || *callee != CallSource::Native(exe.lib(lib).specialization.function)
                {
                    return Err(Refused::Source);
                }
                // Binding authenticates the installed value-only, zero-scratch
                // native contract. Its canonical peak/delta is this result's
                // exact Holding, not an amount chosen by the consumer.
                polynomial.term(3 * index, *coefficient)?;
            }
            _ => return Err(Refused::Source),
        }
    }
    Ok(polynomial)
}
pub(super) fn compile(exe: &Executable, nodes: &[Node]) -> Result<Box<[Polynomial]>, Refused> {
    let mut delta = Polynomial::default();
    let mut peaks = Vec::with_capacity(2 * nodes.len());
    for (index, node) in nodes.iter().enumerate() {
        let mut peak = delta.clone();
        peak.add(&amount(exe, nodes, index, &node.event.peak)?, 1)?;
        peaks.push(peak);
        delta.add(&amount(exe, nodes, index, &node.event.delta)?, 1)?;
        peaks.push(delta.clone());
    }
    Ok(peaks.into())
}
