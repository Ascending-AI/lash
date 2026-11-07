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
    fn snapshot(&mut self, snapshot: &PrefixSnapshot) -> Option<PrefixFacts> {
        let mut facts = snapshot.facts.clone()?;
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
        Some(facts)
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
        let equal = source.zip(isa).is_some_and(|(source, isa)| {
            match (&source.canonical, &isa.canonical) {
                (Some(source), Some(isa)) => source == isa,
                (None, None) => source_ids
                    .snapshot(source)
                    .zip(isa_ids.snapshot(isa))
                    .is_some_and(|(source, isa)| source == isa),
                _ => false,
            }
        });
        if !equal {
            let evidence = match source.zip(isa) {
                Some((source, isa)) if source.canonical.is_some() && isa.canonical.is_some() => {
                    disagreements(std::slice::from_ref(source), std::slice::from_ref(isa)).first().map_or_else(|| "canonical mismatch".into(), |(_, field, expected, actual)| format!("{field}: source={expected}; region={actual}"))
                },
                _ => "prefix-exact values, identity, graph, resource, charge or continuation mismatch".into(),
            };
            return Err(QualificationFailure {
                prefix,
                layout: Some(layout),
                expected: source.cloned().map(Box::new),
                actual: isa.cloned().map(Box::new),
                evidence,
                reached: Vec::new(),
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
            if !witness.complete {
                return Err(QualificationFailure::unavailable(
                    "complete ISA structural counters are not instrumented",
                ));
            }
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
        if let Some(canonical) = &prefix.canonical {
            *coverage
                .entry(format!("stop/{}", canonical.stop))
                .or_default() += 1;
            continue;
        }
        let Some(facts) = prefix.facts.as_ref() else {
            return Err(QualificationFailure::unavailable(
                "a snapshot omitted its observations",
            ));
        };
        if let Some((kind, _)) = &facts.failure {
            *coverage.entry(format!("failure/{kind}")).or_default() += 1;
        }
        if let Some(interruption) = facts.interruption {
            *coverage
                .entry(format!("interruption/{interruption:?}"))
                .or_default() += 1;
        }
        *coverage
            .entry(format!("depth/{}", facts.depth))
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

/// Reports the first differing prefix for each independently observed field.
/// Full observations remain attached to QualificationFailure; this projection
/// makes large graph disagreements readable without replacing exact equality.
pub(super) fn disagreements(
    source: &[PrefixSnapshot],
    isa: &[PrefixSnapshot],
) -> Vec<(usize, String, String, String)> {
    let mut first = BTreeMap::new();
    for prefix in 0..source.len().max(isa.len()) {
        let pair = source
            .get(prefix)
            .and_then(PrefixSnapshot::canonical)
            .zip(isa.get(prefix).and_then(PrefixSnapshot::canonical));
        let Some((source, isa)) = pair else {
            first.entry("prefix presence".into()).or_insert((
                prefix,
                format!("{} prefixes", source.len()),
                format!("{} prefixes", isa.len()),
            ));
            continue;
        };
        let mut field = |name: &str, equal: bool, expected: String, actual: String| {
            if !equal {
                first
                    .entry(name.into())
                    .or_insert((prefix, expected, actual));
            }
        };
        field(
            "charge",
            source.meters.charged == isa.meters.charged,
            format!("{:?}", source.meters.charged),
            format!("{:?}", isa.meters.charged),
        );
        field(
            "peak",
            source.peak == isa.peak,
            source.peak.to_string(),
            isa.peak.to_string(),
        );
        field(
            "memory",
            source.meters.memory == isa.meters.memory,
            source.meters.memory.to_string(),
            isa.meters.memory.to_string(),
        );
        field(
            "live tasks",
            source.meters.live_tasks == isa.meters.live_tasks,
            source.meters.live_tasks.to_string(),
            isa.meters.live_tasks.to_string(),
        );
        field(
            "stop",
            source.stop == isa.stop,
            source.stop.clone(),
            isa.stop.clone(),
        );
        field(
            "stop payload",
            source.event == isa.event,
            format!("{:?}", source.event),
            format!("{:?}", isa.event),
        );
        field(
            "charges",
            source.charges == isa.charges,
            format!("{:?}", source.charges),
            format!("{:?}", isa.charges),
        );
        field(
            "root holdings",
            source.resources.roots == isa.resources.roots,
            format!("{:?}", source.resources.roots),
            format!("{:?}", isa.resources.roots),
        );
        field(
            "entity holdings",
            source.resources.entities == isa.resources.entities,
            format!("{:?}", source.resources.entities),
            format!("{:?}", isa.resources.entities),
        );
        field(
            "balance",
            source.resources.balance == isa.resources.balance,
            format!("{:?}", source.resources.balance),
            format!("{:?}", isa.resources.balance),
        );
        field(
            "next invocation",
            source.resources.next_invocation == isa.resources.next_invocation,
            source.resources.next_invocation.to_string(),
            isa.resources.next_invocation.to_string(),
        );
        for (name, expected, actual) in [
            (
                "continuation",
                source.state.continuation(),
                isa.state.continuation(),
            ),
            ("graph", source.state.graph(), isa.state.graph()),
        ] {
            if expected != actual {
                let at = expected
                    .iter()
                    .zip(actual)
                    .position(|(a, b)| a != b)
                    .unwrap_or(expected.len().min(actual.len()));
                field(
                    name,
                    false,
                    format!("row {at}/{}: {:?}", expected.len(), expected.get(at)),
                    format!("row {at}/{}: {:?}", actual.len(), actual.get(at)),
                );
            }
        }
        match (&source.session, &isa.session) {
            (Some(source), Some(isa)) => {
                for (name, expected, actual) in [
                    (
                        "session continuation",
                        source.continuation(),
                        isa.continuation(),
                    ),
                    ("session graph", source.graph(), isa.graph()),
                ] {
                    if expected != actual {
                        let at = expected
                            .iter()
                            .zip(actual)
                            .position(|(a, b)| a != b)
                            .unwrap_or(expected.len().min(actual.len()));
                        field(
                            name,
                            false,
                            format!("row {at}/{}: {:?}", expected.len(), expected.get(at)),
                            format!("row {at}/{}: {:?}", actual.len(), actual.get(at)),
                        );
                    }
                }
            }
            (None, None) => {}
            _ => field(
                "session presence",
                false,
                source.session.is_some().to_string(),
                isa.session.is_some().to_string(),
            ),
        }
        field(
            "printed",
            source.printed == isa.printed,
            format!("{:?}", source.printed),
            format!("{:?}", isa.printed),
        );
        field(
            "interruption",
            source.interruption == isa.interruption,
            format!("{:?}", source.interruption),
            format!("{:?}", isa.interruption),
        );
    }
    first
        .into_iter()
        .map(|(field, (prefix, expected, actual))| (prefix, field, expected, actual))
        .collect()
}
