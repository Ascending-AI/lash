//! The identity correspondence lasts for the complete trace, including released
//! identities. Restarting it at each prefix would accept stale substitutions.
use super::*;
use std::collections::BTreeMap;

#[derive(Default)]
struct Identities {
    seen: BTreeMap<Identity, Identity>,
    next: BTreeMap<IdentityKind, u64>,
}

impl Identities {
    fn identity(&mut self, id: &mut Identity) {
        if id.kind == IdentityKind::ExistingEntity {
            return;
        }
        *id = *self.seen.entry(*id).or_insert_with(|| {
            let next = self.next.entry(id.kind).or_default();
            let canonical = Identity {
                kind: id.kind,
                id: *next,
            };
            *next += 1;
            canonical
        });
    }
    fn coordinate(&mut self, at: &mut Coordinate) {
        self.identity(&mut at.invocation);
        self.identity(&mut at.occurrence);
    }
    fn owner(&mut self, owner: &mut Owner) {
        if let Owner::Frame { invocation, .. } | Owner::Value { invocation, .. } = owner {
            let mut id = Identity {
                kind: IdentityKind::Invocation,
                id: *invocation,
            };
            self.identity(&mut id);
            *invocation = id.id;
        }
    }
    fn value(&mut self, value: &mut SemanticValue) {
        match value {
            SemanticValue::Reference(id) | SemanticValue::Object { identity: id, .. } => {
                self.identity(id)
            }
            SemanticValue::Tuple(values) => {
                for value in values {
                    self.value(value);
                }
            }
            SemanticValue::Scalar(_) => {}
        }
    }
    fn charge(&mut self, charge: &mut ChargeEntry) {
        self.coordinate(&mut charge.at);
    }
    fn snapshot(&mut self, snapshot: &PrefixSnapshot) -> PrefixFacts {
        let mut facts = snapshot.facts.clone();
        facts.values.sort_by_key(|(role, _, _)| *role);
        facts.roots.sort_by_key(|root| root.owner);
        self.coordinate(&mut facts.at);
        for (owner, value, provenance) in &mut facts.values {
            self.owner(owner);
            self.value(value);
            self.identity(provenance);
        }
        for origin in &mut facts.origins {
            self.identity(&mut origin.value);
            self.coordinate(&mut origin.at);
            for input in &mut origin.inputs {
                self.identity(input);
            }
        }
        for root in &mut facts.roots {
            self.owner(&mut root.owner);
            for (id, _) in &mut root.edges {
                self.identity(id);
            }
            root.edges.sort();
        }
        for (owner, _, target, _) in &mut facts.edges {
            self.identity(owner);
            self.identity(target);
        }
        for (id, _) in &mut facts.retained {
            self.identity(id);
        }
        for id in &mut facts.released {
            self.identity(id);
        }
        for (id, _, _) in &mut facts.dependencies {
            self.identity(id);
        }
        for charge in &mut facts.charges {
            self.charge(charge);
        }
        for charge in &mut facts.debt {
            self.charge(charge);
        }
        for (id, _) in &mut facts.call_offsets {
            self.identity(id);
        }
        if let Some((_, at)) = &mut facts.failure {
            self.coordinate(at);
        }
        if let Some(value) = &mut facts.stop_payload {
            self.value(value);
        }
        for value in &mut facts.operands {
            self.value(value);
        }
        if let Some(value) = &mut facts.pending {
            self.value(value);
        }
        facts.origins.sort_by_key(|origin| origin.value);
        facts.values.sort_by_key(|(owner, _, _)| *owner);
        facts.roots.sort_by_key(|root| root.owner);
        facts.edges.sort();
        facts.retained.sort_by_key(|(id, _)| *id);
        facts.released.sort();
        facts.dependencies.sort();
        facts
    }
}

pub(super) fn compare(
    expected: &[PrefixSnapshot],
    actual: &[PrefixSnapshot],
    layout: IsaLayout,
) -> Result<(), QualificationFailure> {
    let mut source_ids = Identities::default();
    let mut isa_ids = Identities::default();
    for prefix in 0..expected.len().max(actual.len()) {
        let source = expected.get(prefix);
        let isa = actual.get(prefix);
        let equal = source
            .zip(isa)
            .is_some_and(|(source, isa)| source_ids.snapshot(source) == isa_ids.snapshot(isa));
        if !equal {
            return Err(QualificationFailure {
                prefix,
                layout: Some(layout),
                expected: source.cloned().map(Box::new),
                actual: isa.cloned().map(Box::new),
                evidence: "prefix-exact values, identity, graph, resource, charge or continuation mismatch".into(),
            });
        }
    }
    if expected.is_empty() {
        return Err(QualificationFailure::unavailable(
            "source trace omitted the initial legal prefix",
        ));
    }
    Ok(())
}

pub(super) fn qualify_observations(
    source: &[PrefixSnapshot],
    mut runner: impl FnMut(IsaLayout) -> Result<Vec<PrefixSnapshot>, QualificationFailure>,
    fixture_only: bool,
) -> Result<QualificationReport, QualificationFailure> {
    let layouts = [IsaLayout::Generic, IsaLayout::Specialized];
    let mut witnesses = Vec::new();
    for layout in layouts {
        let actual = runner(layout)?;
        compare(source, &actual, layout)?;
        if !fixture_only {
            let Some(witness) = actual.last().and_then(|prefix| prefix.witness.as_ref()) else {
                return Err(QualificationFailure::unavailable(
                    "ISA execution counters were not observed; prefix agreement alone is incomplete",
                ));
            };
            if witness.interior_root_transactions != 0
                || witness.source_fallbacks != 0
                || witness.interior_roots != 0
                || witness.interior_ledger_calls != 0
                || witness.interior_transactions != 0
                || witness.authentication_hashes != 0
                || witness.table_snapshots != 0
                || witness.phase_proof_allocations != 0
                || (source.len() > 1 && witness.verified_operations == 0)
            {
                return Err(QualificationFailure::unavailable(
                    "ISA structural witness observed interior root transactions, source fallback, or no verified operations",
                ));
            }
            witnesses.push((layout, witness.clone()));
        }
    }
    let mut coverage = BTreeMap::new();
    for prefix in source {
        if let Some((kind, _)) = &prefix.facts.failure {
            *coverage.entry(format!("failure/{kind}")).or_default() += 1;
        }
        if let Some(interruption) = prefix.facts.interruption {
            *coverage
                .entry(format!("interruption/{interruption:?}"))
                .or_default() += 1;
        }
        *coverage
            .entry(format!("depth/{}", prefix.facts.depth))
            .or_default() += 1;
    }
    Ok(QualificationReport {
        reached_prefixes: source.len(),
        layouts,
        fixture_only,
        coverage,
        witnesses,
    })
}
