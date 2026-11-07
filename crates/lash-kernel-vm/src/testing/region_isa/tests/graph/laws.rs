use super::*;

fn scenes() -> Vec<Scene> {
    vec![
        Scene::new(
            vec![
                (Kind::List, vec![(Key::Int(0), Val::Ref(1))], false),
                (Kind::List, vec![(Key::Int(0), Val::Ref(0))], false),
            ],
            vec![],
            vec![
                Op::Allocate {
                    to: 0,
                    kind: Kind::List,
                    members: members(vec![]),
                },
                Op::Root { to: 0, value: None },
            ],
        ),
        Scene::new(
            vec![],
            vec![],
            vec![
                Op::Allocate {
                    to: 0,
                    kind: Kind::List,
                    members: members(vec![(Key::Int(0), Val::Int(17))]),
                },
                Op::Allocate {
                    to: 1,
                    kind: Kind::List,
                    members: members(vec![(Key::Int(0), tuple(vec![Val::Ref(0), Val::Ref(0)]))]),
                },
                Op::Root {
                    to: 2,
                    value: Some(tuple(vec![
                        Val::Ref(1),
                        tuple(vec![Val::Ref(0), Val::Ref(1)]),
                    ])),
                },
                Op::Root { to: 0, value: None },
                Op::Root { to: 1, value: None },
                Op::Member {
                    object: 1,
                    key: Key::Int(0),
                    next: Some(Val::Ref(0)),
                },
                Op::Root { to: 2, value: None },
                Op::Allocate {
                    to: 3,
                    kind: Kind::Set,
                    members: members(vec![(key("new"), Val::Absent)]),
                },
                Op::Root { to: 3, value: None },
                Op::Allocate {
                    to: 4,
                    kind: Kind::Record,
                    members: members(vec![(key("bytes"), Val::Bytes(vec![1, 2, 3].into()))]),
                },
            ],
        ),
        Scene::new(
            vec![],
            vec![],
            vec![
                Op::Allocate {
                    to: 0,
                    kind: Kind::List,
                    members: members(vec![]),
                },
                Op::Allocate {
                    to: 1,
                    kind: Kind::List,
                    members: members(vec![]),
                },
                Op::Member {
                    object: 0,
                    key: Key::Int(0),
                    next: Some(Val::Ref(1)),
                },
                Op::Member {
                    object: 1,
                    key: Key::Int(0),
                    next: Some(Val::Ref(0)),
                },
                Op::Root { to: 0, value: None },
                Op::Root { to: 1, value: None },
                Op::Allocate {
                    to: 2,
                    kind: Kind::Cell,
                    members: members(vec![(Key::Int(0), Val::Null)]),
                },
                Op::Member {
                    object: 2,
                    key: Key::Int(0),
                    next: Some(Val::Ref(2)),
                },
                Op::Root { to: 2, value: None },
            ],
        ),
        Scene::new(
            vec![
                (
                    Kind::Map,
                    vec![(Key::Int(1), Val::Text("entry".into()))],
                    true,
                ),
                (Kind::List, vec![(Key::Int(0), Val::Int(88))], true),
            ],
            vec![(0, Val::Ref(0)), (99, Val::Ref(1))],
            vec![
                Op::Read {
                    object: 0,
                    key: Key::Int(1),
                    to: 4,
                },
                Op::Allocate {
                    to: 1,
                    kind: Kind::Cell,
                    members: members(vec![(Key::Int(0), Val::Ref(0))]),
                },
                Op::Member {
                    object: 0,
                    key: Key::Float(1),
                    next: Some(Val::Ref(2)),
                },
                Op::Read {
                    object: 0,
                    key: Key::Int(1),
                    to: 4,
                },
                Op::Root {
                    to: 5,
                    value: Some(tuple(vec![Val::Ref(0), Val::Ref(2), Val::Ref(0)])),
                },
                Op::Enter,
                Op::Enter,
                Op::Callback {
                    object: 0,
                    key: Key::Float(1),
                    to: 6,
                    external: false,
                },
                Op::Leave,
                Op::Leave,
                Op::Callback {
                    object: 0,
                    key: Key::Int(1),
                    to: 7,
                    external: true,
                },
                Op::Root { to: 0, value: None },
                Op::Root { to: 1, value: None },
                Op::Root { to: 4, value: None },
                Op::Root { to: 5, value: None },
                Op::Root { to: 6, value: None },
                Op::Root { to: 7, value: None },
            ],
        ),
        Scene::new(
            vec![(
                Kind::Map,
                vec![(Key::Int(1), Val::Int(9)), (key("tail"), Val::Null)],
                false,
            )],
            vec![(0, Val::Ref(0))],
            vec![
                Op::Member {
                    object: 0,
                    key: Key::Float(1),
                    next: Some(Val::Text("replace".into())),
                },
                Op::Walk {
                    object: 0,
                    after: None,
                    to: 1,
                },
                Op::Member {
                    object: 0,
                    key: Key::Int(1),
                    next: None,
                },
                Op::Member {
                    object: 0,
                    key: Key::Float(1),
                    next: Some(Val::Int(9)),
                },
                Op::Walk {
                    object: 0,
                    after: Some(0),
                    to: 2,
                },
                Op::Grow {
                    object: 0,
                    additional: 4096,
                },
                Op::Member {
                    object: 0,
                    key: key("tail"),
                    next: None,
                },
                Op::Member {
                    object: 0,
                    key: key("tail"),
                    next: Some(Val::Null),
                },
                Op::Root { to: 0, value: None },
            ],
        ),
        Scene::new(
            vec![],
            vec![],
            vec![
                Op::Allocate {
                    to: 0,
                    kind: Kind::Cell,
                    members: members(vec![(Key::Int(0), Val::Int(-129))]),
                },
                Op::Allocate {
                    to: 1,
                    kind: Kind::Closure,
                    members: members(vec![(Key::Int(0), Val::Ref(0)), (Key::Int(1), Val::Ref(0))]),
                },
                Op::Root { to: 0, value: None },
                Op::Enter,
                Op::Enter,
                Op::Enter,
                Op::Allocate {
                    to: 8,
                    kind: Kind::List,
                    members: members(vec![(Key::Int(0), Val::Ref(1))]),
                },
                Op::Root { to: 8, value: None },
                Op::Read {
                    object: 1,
                    key: Key::Int(0),
                    to: 2,
                },
                Op::Member {
                    object: 0,
                    key: Key::Int(0),
                    next: Some(tuple(vec![Val::Int(0), Val::Bytes(vec![5; 64].into())])),
                },
                Op::Callback {
                    object: 0,
                    key: Key::Int(0),
                    to: 3,
                    external: false,
                },
                Op::Leave,
                Op::Leave,
                Op::Leave,
                Op::Root { to: 1, value: None },
                Op::Root { to: 2, value: None },
            ],
        ),
    ]
}

fn probe(scene: &Scene, stop: usize) -> usize {
    let expected = oracle::trace(scene, stop);
    let report = comparison::qualify_observations(
        &expected,
        |layout| {
            let (mut actual, stats) = local::trace(scene, layout, stop);
            let retained_high_water = expected
                .iter()
                .map(|prefix| prefix.facts.as_ref().unwrap().retained.len())
                .max()
                .unwrap();
            assert_eq!(
                stats.max_slots, retained_high_water,
                "a refused phase reserved a target slot or storage kept elapsed allocations"
            );
            let entry = actual[0].clone();
            let mut canonical = super::super::authority::FakeLedger::new(entry.clone());
            assert_eq!(canonical.snapshot(), &entry);
            canonical
                .publish(actual.last().unwrap().clone(), None)
                .unwrap();
            *actual.last_mut().unwrap() = canonical.snapshot().clone();
            // Includes refused traces. The committed control contains only the
            // affordable earlier phases and the exact current stop payload.
            Ok(actual)
        },
        true,
    )
    .unwrap_or_else(|error| panic!("stop {stop}, scene {scene:?}: {error:?}"));
    assert!(report.fixture_only());
    report.reached_prefixes()
}

/// ISA-GRAPH-PREFIX: preserve duplicate/nested aliases, source RC cycles,
/// original key spelling, sequence/version changes, and transient growth room.
/// Each current prefix is reached in a new run before it is materialized.
#[test]
fn allocation_edit_release_prefixes_match_itemized_source() {
    let mut cases = 0;
    let mut prefixes = 0;
    for scene in scenes() {
        let mut failing = scene.clone();
        // A post-edit missing/dead-member failure must preserve the exact
        // accepted graph rather than reconstructing entry or running ahead.
        failing.ops.push(Op::Read {
            object: 0,
            key: Key::Int(404),
            to: 31,
        });
        prefixes += probe(&failing, failing.ops.len());
        cases += 1;
        let expected = oracle::trace(&scene, scene.ops.len());
        for stop in 0..=scene.ops.len() {
            prefixes += probe(&scene, stop);
            cases += 1;
        }
        for reached in &expected {
            for delta in [-1_i64, 0, 1] {
                let cutoff = |value: u64| value.saturating_add_signed(delta);
                let charged = reached
                    .facts
                    .as_ref()
                    .unwrap()
                    .charges
                    .iter()
                    .map(|entry| entry.units)
                    .sum();
                for bounded in [
                    Scene {
                        charge: cutoff(charged),
                        ..scene.clone()
                    },
                    Scene {
                        memory: cutoff(reached.facts.as_ref().unwrap().peak),
                        ..scene.clone()
                    },
                    Scene {
                        depth: cutoff(u64::from(reached.facts.as_ref().unwrap().depth)) as u32,
                        ..scene.clone()
                    },
                ] {
                    prefixes += probe(&bounded, bounded.ops.len());
                    cases += 1;
                }
            }
        }
    }
    let overflowing = Scene::new(
        vec![(Kind::List, vec![], false)],
        vec![(0, Val::Ref(0))],
        vec![Op::Grow {
            object: 0,
            additional: usize::MAX,
        }],
    );
    prefixes += probe(&overflowing, 1);
    cases += 1;
    eprintln!(
        "fixture-only ISA-GRAPH-PREFIX: {cases} independently stopped/cutoff cases, {prefixes} source prefixes, both layouts"
    );
}

/// ISA-GRAPH-ATOMIC: abandonment around every local phase and failed prepare
/// or commit preserve all canonical facts; committing installs only that prefix.
#[test]
fn abandoned_graph_phases_and_failed_publications_preserve_entry() {
    let mut injections = 0;
    for scene in scenes() {
        for layout in [IsaLayout::Generic, IsaLayout::Specialized] {
            let source = oracle::trace(&scene, scene.ops.len());
            for stop in 0..=scene.ops.len() {
                for after in [false, true] {
                    let reached = if after { stop } else { stop.saturating_sub(1) };
                    let (trace, _) = local::trace(&scene, layout, reached);
                    comparison::compare(&source[..=reached], &trace, layout).unwrap();
                    let entry = trace[0].clone();
                    let next = trace.last().unwrap().clone();
                    for fault in [
                        Interruption::BeforePrepareFailure,
                        Interruption::BeforeCommitFailure,
                    ] {
                        let mut canonical = super::super::authority::FakeLedger::new(entry.clone());
                        assert_eq!(
                            canonical.snapshot(),
                            &entry,
                            "private local phase changed canonical state"
                        );
                        assert!(canonical.publish(next.clone(), Some(fault)).is_err());
                        assert_eq!(
                            canonical.snapshot(),
                            &entry,
                            "failed graph preparation changed canonical state"
                        );
                        canonical.publish(next.clone(), None).unwrap();
                        let mut committed_trace = trace.clone();
                        *committed_trace.last_mut().unwrap() = canonical.snapshot().clone();
                        comparison::compare(&source[..=reached], &committed_trace, layout).unwrap();
                        injections += 1;
                    }
                }
            }
        }
    }
    eprintln!(
        "fixture-only ISA-GRAPH-ATOMIC: {injections} phase/publication failures with committed-prefix controls"
    );
}

/// ISA-GRAPH-CORRESPONDENCE: each new graph proof obligation has a separate
/// mutation which the independent full-prefix observations must reject.
#[test]
fn graph_corruptions_cannot_hide_behind_equal_prices_or_final_roots() {
    type Mutant = fn(&mut PrefixFacts);
    let mutations: &[(&str, Mutant)] = &[
        ("identity mapping", |facts| {
            if let SemanticValue::Object { identity, .. } = &mut facts.values[0].1 {
                identity.id += 50;
            }
        }),
        ("duplicate member edge", |facts| facts.edges[0].3 += 1),
        ("local version", |facts| facts.dependencies[0].2 += 1),
        ("body spelling/order/revision", |facts| {
            facts.retained[0].1.push(99)
        }),
        ("source release", |facts| {
            facts.released.push(id(IdentityKind::Entity, 999_999))
        }),
        ("transient peak", |facts| facts.peak -= 1),
        ("charge amount", |facts| facts.charges[0].units += 1),
        ("identity allocation progression", |facts| {
            facts.continuation[15] ^= 1
        }),
        ("root multiplicity", |facts| {
            facts
                .roots
                .iter_mut()
                .find(|root| !root.edges.is_empty())
                .unwrap()
                .edges[0]
                .1 += 1
        }),
        ("fresh retained cycle", |facts| {
            facts.retained.pop();
        }),
    ];
    let scene = scenes().remove(2);
    let source = oracle::trace(&scene, scene.ops.len());
    for layout in [IsaLayout::Generic, IsaLayout::Specialized] {
        for (name, mutate) in mutations {
            let (mut trace, _) = local::trace(&scene, layout, scene.ops.len());
            mutate(trace[4].facts.as_mut().unwrap());
            comparison::compare(&source, &trace, layout).expect_err(name);
        }
        let (mut trace, _) = local::trace(&scene, layout, scene.ops.len());
        trace
            .last_mut()
            .unwrap()
            .facts
            .as_mut()
            .unwrap()
            .retained
            .clear();
        comparison::compare(&source, &trace, layout).expect_err("rootless retained cycles omitted");
        let old_scene = scenes().remove(3);
        let old_source = oracle::trace(&old_scene, old_scene.ops.len());
        let (mut old_trace, _) = local::trace(&old_scene, layout, old_scene.ops.len());
        if let SemanticValue::Object { identity, .. } =
            &mut old_trace[1].facts.as_mut().unwrap().values[0].1
        {
            identity.id += 10_000;
        }
        comparison::compare(&old_source, &old_trace, layout)
            .expect_err("existing identity cannot be renamed");
    }
    eprintln!(
        "fixture-only ISA-GRAPH-CORRESPONDENCE: {} graph corruptions rejected in both layouts",
        mutations.len() + 2
    );
}

/// ISA-LOCAL-STORAGE: repeated edits retain current contents and counters,
/// last-owner release reuses slots without reusing identities or generations.
/// Structural observations are attached to semantic oracle comparisons.
#[test]
fn repeated_edits_slot_reuse_and_callbacks_keep_current_graph() {
    let mut scene = Scene::new(
        vec![(Kind::Map, vec![], true), (Kind::List, vec![], true)],
        vec![(0, Val::Ref(0)), (99, Val::Ref(1))],
        vec![],
    );
    for iteration in 0..64 {
        scene.ops.extend([
            Op::Allocate {
                to: 1,
                kind: Kind::Cell,
                members: members(vec![(Key::Int(0), Val::Int(iteration))]),
            },
            Op::Member {
                object: 0,
                key: key("current"),
                next: Some(Val::Ref(2 + iteration as u64)),
            },
            Op::Member {
                object: 0,
                key: key("current"),
                next: Some(Val::Ref(2 + iteration as u64)),
            },
            Op::Callback {
                object: 0,
                key: key("current"),
                to: 2,
                external: iteration % 2 == 0,
            },
            Op::Member {
                object: 0,
                key: key("current"),
                next: None,
            },
            Op::Root { to: 1, value: None },
            Op::Root { to: 2, value: None },
        ]);
    }
    for number in 0..80 {
        scene.ops.push(Op::Member {
            object: 0,
            key: Key::Int(number),
            next: Some(Val::Int(number)),
        });
    }
    scene.ops.push(Op::Grow {
        object: 0,
        additional: 8192,
    });
    for number in 0..80 {
        scene.ops.push(Op::Member {
            object: 0,
            key: Key::Float(number),
            next: None,
        });
    }
    let source = oracle::trace(&scene, scene.ops.len());
    for layout in [IsaLayout::Generic, IsaLayout::Specialized] {
        let mut runner = local::Local::new(&scene, layout);
        let mut out = vec![runner.snapshot(0, None)];
        let mut stale = None;
        let entry = out[0].clone();
        let mut canonical = super::super::authority::FakeLedger::new(entry);
        let mut durable = canonical.snapshot().clone();
        for (index, op) in scene.ops.iter().enumerate() {
            // An actual escape first publishes the preceding graph; a pure
            // callback observes the current local body without publication.
            if matches!(op, Op::Callback { external: true, .. }) {
                canonical
                    .publish(runner.snapshot(index, None), None)
                    .unwrap();
                durable = canonical.snapshot().clone();
            }
            if let Op::Callback { external, .. } = op {
                let observed = if *external {
                    canonical.snapshot().clone()
                } else {
                    runner.snapshot(index, None)
                };
                let mut callback_trace = out.clone();
                *callback_trace.last_mut().unwrap() = observed.clone();
                comparison::compare(&source[..=index], &callback_trace, layout).unwrap();
                runner.stats.callbacks.push(observed);
            }
            runner.step(&scene, op).unwrap();
            if matches!(op, Op::Allocate { .. }) {
                if let Some(handle) = stale {
                    assert_eq!(
                        runner.resolve(handle),
                        None,
                        "reused physical slot accepted stale generation"
                    );
                }
                stale = Some(runner.handle(2 + (index / 7) as u64));
            }
            out.push(runner.snapshot(index + 1, None));
        }
        comparison::compare(&source, &out, layout).unwrap();
        assert_eq!(
            runner.stats.max_slots, 3,
            "physical slot storage follows retained high water, not elapsed allocations"
        );
        assert_eq!(
            runner.world.objects.len(),
            source
                .last()
                .unwrap()
                .facts
                .as_ref()
                .unwrap()
                .retained
                .len(),
            "dead body metadata survived release"
        );
        assert_eq!(
            runner.stats.max_members, 80,
            "storage follows the live member high water"
        );
        assert!(
            runner.world.objects[&0].body.entries.is_empty(),
            "delete/reinsert history survived in the current body"
        );
        assert_eq!(
            runner.stats.warmed,
            BTreeSet::from([0]),
            "unrelated cold body was warmed"
        );
        assert_eq!(runner.stats.callbacks.len(), 64);
        assert_ne!(
            durable, out[0],
            "external callback never received an actual prefix publication"
        );
    }
    eprintln!(
        "fixture-only ISA-LOCAL-STORAGE: 64 allocate/edit/callback/delete/release iterations and 80-key growth per layout, compared to canonical itemized retention"
    );
}
