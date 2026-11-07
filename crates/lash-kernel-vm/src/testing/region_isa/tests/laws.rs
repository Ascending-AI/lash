use super::*;

fn assert_qualifies(case: &Case) -> usize {
    let source = source_oracle_fixture(case);
    let report =
        comparison::qualify_observations(&source, |layout| Ok(isa_fixture(case, layout)), true)
            .unwrap_or_else(|error| panic!("{case:?}: {error:?}"));
    assert!(
        report.fixture_only,
        "fixture agreement must never qualify the cutover"
    );
    report.reached_prefixes
}

/// ISA-PREFIX-EXACT: independent itemized holdings and semantic execution must
/// match both fake layouts at every loop occurrence and tight limit prefix.
#[test]
fn generated_prefixes_match_both_fake_layouts() {
    let mut cases = 0;
    let mut prefixes = 0;
    for loops in 0..=3 {
        for input in [-2, 0, 7, i64::MAX - 2, i64::MAX] {
            for alias in [false, true] {
                for cycle in [false, true] {
                    let base = Case {
                        loops,
                        input,
                        alias,
                        cycle,
                        ..roomy()
                    };
                    let expected = source_oracle_fixture(&base);
                    prefixes += assert_qualifies(&base);
                    cases += 1;
                    prefixes += assert_qualifies(&Case {
                        schedule: expected
                            .iter()
                            .enumerate()
                            .map(|(ordinal, _)| (ordinal as u64, Interruption::SealReopen))
                            .collect(),
                        ..base.clone()
                    });
                    cases += 1;
                    for (ordinal, reached) in expected.iter().enumerate() {
                        let charged: u64 = reached
                            .facts
                            .as_ref()
                            .unwrap()
                            .charges
                            .iter()
                            .map(|c| c.units)
                            .sum();
                        for delta in [-1_i64, 0, 1] {
                            let cutoff = |n: u64| n.saturating_add_signed(delta);
                            for bounded in [
                                Case {
                                    charge: cutoff(charged),
                                    ..base.clone()
                                },
                                Case {
                                    memory: cutoff(reached.facts.as_ref().unwrap().peak),
                                    ..base.clone()
                                },
                                Case {
                                    depth: cutoff(u64::from(reached.facts.as_ref().unwrap().depth))
                                        as u32,
                                    ..base.clone()
                                },
                            ] {
                                prefixes += assert_qualifies(&bounded);
                                cases += 1;
                            }
                        }
                        for interruption in [
                            Interruption::Slice,
                            Interruption::Observer,
                            Interruption::Handoff,
                            Interruption::BeforePrepareFailure,
                            Interruption::BeforeCommitFailure,
                            Interruption::SealReopen,
                        ] {
                            prefixes += assert_qualifies(&Case {
                                schedule: vec![(ordinal as u64, interruption)],
                                ..base.clone()
                            });
                            cases += 1;
                        }
                        if ordinal > 0 {
                            prefixes += assert_qualifies(&Case {
                                throw: Some(ordinal),
                                ..base.clone()
                            });
                            cases += 1;
                        }
                    }
                }
            }
        }
    }
    eprintln!(
        "fixture-only ISA-PREFIX-EXACT: {cases} cases, {prefixes} reached prefixes, both layouts"
    );
}

/// ISA-CORRESPONDENCE: each proof corruption must change the independently
/// visible prefix or refuse. Price equality cannot excuse substituted facts.
#[test]
fn corrupted_prefixes_are_rejected_by_their_observed_facts() {
    type Mutation = fn(&mut Vec<PrefixSnapshot>);
    let mutants: &[(&str, Mutation)] = &[
        ("foreign lease", |s| {
            s[2].facts.as_mut().unwrap().dependencies[0].0.id += 1
        }),
        ("stale lease", |s| {
            s[2].facts.as_mut().unwrap().dependencies[0].1 -= 1
        }),
        ("sibling draft", |s| {
            s[2].facts.as_mut().unwrap().dependencies[0].2 += 1
        }),
        ("equal-price substitution", |s| {
            s[2].facts.as_mut().unwrap().values[0].1 = scalar(99)
        }),
        ("duplicate occurrence", |s| {
            s.insert(2, s[1].clone());
        }),
        ("skipped occurrence", |s| {
            s.remove(2);
        }),
        ("debt substitution", |s| {
            s[2].facts.as_mut().unwrap().debt[0].units += 1
        }),
        ("changed dependency", |s| {
            s[2].facts.as_mut().unwrap().dependencies[1].2 += 1
        }),
        ("mutated peak", |s| s[2].facts.as_mut().unwrap().peak -= 1),
        ("owner edge", |s| {
            s[2].facts
                .as_mut()
                .unwrap()
                .roots
                .iter_mut()
                .find(|root| !root.edges.is_empty())
                .unwrap()
                .edges[0]
                .1 += 1;
        }),
        ("owner identity", |s| {
            s[2].facts.as_mut().unwrap().roots[0].owner = Owner::Frame {
                task: TaskId(99),
                invocation: 0,
            }
        }),
        ("root holding", |s| {
            s[2].facts.as_mut().unwrap().roots[0].direct_bytes += 1
        }),
        ("call offset", |s| {
            s[2].facts.as_mut().unwrap().call_offsets[0].1 += 1
        }),
        ("value provenance", |s| {
            s[2].facts.as_mut().unwrap().values[0].2 = s[1].facts.as_ref().unwrap().values[0].2
        }),
        ("producer position", |s| {
            s[2].facts.as_mut().unwrap().origins[0].at.site += 99
        }),
        ("producer kernel", |s| {
            s[2].facts.as_mut().unwrap().origins[0].kernel = b"multiply".to_vec()
        }),
        ("producer input", |s| {
            s[2].facts.as_mut().unwrap().origins[0].inputs[0] = id(IdentityKind::Provenance, 999)
        }),
        ("charge order", |s| {
            s[2].facts.as_mut().unwrap().charges.swap(0, 1)
        }),
        ("debt cursor", |s| {
            s[2].facts.as_mut().unwrap().debt_cursor += 1
        }),
        ("source occurrence", |s| {
            s[2].facts.as_mut().unwrap().at.source_occurrence += 1
        }),
        ("operand", |s| {
            s[3].facts.as_mut().unwrap().operands[0] = scalar(999)
        }),
        ("continuation", |s| {
            s[2].facts.as_mut().unwrap().continuation.push(9)
        }),
        ("lost release", |s| {
            s.last_mut()
                .unwrap()
                .facts
                .as_mut()
                .unwrap()
                .released
                .clear();
        }),
    ];
    let case = Case {
        cycle: false,
        ..roomy()
    };
    let source = source_oracle_fixture(&case);
    for layout in [IsaLayout::Generic, IsaLayout::Specialized] {
        for (name, mutate) in mutants {
            let mut actual = isa_fixture(&case, layout);
            mutate(&mut actual);
            let failure = comparison::compare(&source, &actual, layout).expect_err(name);
            assert_eq!(failure.layout, Some(layout), "{name}");
            assert_ne!(failure.expected, failure.actual, "{name}");
        }
    }
    let throwing = Case {
        throw: Some(3),
        ..roomy()
    };
    let expected_throw = source_oracle_fixture(&throwing);
    for layout in [IsaLayout::Generic, IsaLayout::Specialized] {
        let mut actual = isa_fixture(&throwing, layout);
        actual
            .last_mut()
            .unwrap()
            .facts
            .as_mut()
            .unwrap()
            .stop_payload = Some(scalar(99));
        comparison::compare(&expected_throw, &actual, layout)
            .expect_err("equal-price throw payload substitution");
    }
    eprintln!(
        "fixture-only ISA-CORRESPONDENCE: {} corruptions rejected in both layouts",
        mutants.len() + 1
    );
}

/// ISA-IDENTITY: a stable injective renaming is allowed; merging equal-valued
/// graph identities, or changing correspondence after release, is forbidden.
#[test]
fn identity_mapping_is_injective_and_lasts_across_prefixes() {
    let case = roomy();
    let source = source_oracle_fixture(&case);
    let mut renamed = source.clone();
    // Use the same exhaustive visitor as the comparator only to rename every
    // identity. Expected identity/alias behavior still comes from the source.
    for snapshot in &mut renamed {
        rename_facts(snapshot.facts.as_mut().unwrap(), false);
    }
    comparison::compare(&source, &renamed, IsaLayout::Generic).unwrap();
    let mut collapsed = source.clone();
    for snapshot in &mut collapsed {
        rename_facts(snapshot.facts.as_mut().unwrap(), true);
    }
    comparison::compare(&source, &collapsed, IsaLayout::Generic).expect_err("merged entities");
    let mut stale = renamed;
    stale[3].facts.as_mut().unwrap().values[1].1 =
        SemanticValue::Reference(id(IdentityKind::Entity, 999));
    let failure = comparison::compare(&source, &stale, IsaLayout::Specialized).unwrap_err();
    assert_eq!(failure.prefix, 3);
}

fn rename_facts(facts: &mut PrefixFacts, collapse: bool) {
    let rename = |id: &mut Identity| {
        id.id = if collapse && id.kind == IdentityKind::Entity {
            500
        } else {
            id.id + 1000
        };
    };
    let coordinate = |at: &mut Coordinate| {
        rename(&mut at.invocation);
        rename(&mut at.occurrence);
    };
    fn rename_value(v: &mut SemanticValue, rename: &impl Fn(&mut Identity)) {
        match v {
            SemanticValue::Reference(id) | SemanticValue::Object { identity: id, .. } => rename(id),
            SemanticValue::Tuple(values) => {
                for v in values {
                    rename_value(v, rename);
                }
            }
            SemanticValue::Scalar(_) => {}
        }
    }
    let value = |v: &mut SemanticValue| rename_value(v, &rename);
    coordinate(&mut facts.at);
    let owner = |owner: &mut Owner| {
        if let Owner::Frame { invocation, .. } | Owner::Value { invocation, .. } = owner {
            *invocation += 1000;
        }
    };
    for (role, v, provenance) in &mut facts.values {
        owner(role);
        value(v);
        rename(provenance);
    }
    for origin in &mut facts.origins {
        rename(&mut origin.value);
        coordinate(&mut origin.at);
        for input in &mut origin.inputs {
            rename(input);
        }
    }
    for root in &mut facts.roots {
        owner(&mut root.owner);
        for (id, _) in &mut root.edges {
            rename(id);
        }
    }
    for (owner, _, target, _) in &mut facts.edges {
        rename(owner);
        rename(target);
    }
    for (id, _) in &mut facts.retained {
        rename(id);
    }
    for id in &mut facts.released {
        rename(id);
    }
    for (id, _, _) in &mut facts.dependencies {
        rename(id);
    }
    for charge in facts.charges.iter_mut().chain(&mut facts.debt) {
        coordinate(&mut charge.at);
    }
    for (id, _) in &mut facts.call_offsets {
        rename(id);
    }
    if let Some((_, at)) = &mut facts.failure {
        coordinate(at);
    }
    if let Some(payload) = &mut facts.stop_payload {
        value(payload);
    }
    for operand in &mut facts.operands {
        value(operand);
    }
    if let Some(pending) = &mut facts.pending {
        value(pending);
    }
}
