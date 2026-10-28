//! The loop-plan verifiers: lowering admits a scalar loop plan, which feeds
//! the register program's typed repeat, only as its lowered code, its
//! stream's charge regions and its statement units state it, and refuses
//! each misstatement of a summary, of the body's tree and of a `for`'s head;
//! and it admits an island plan only as its stream's charge regions and
//! occurrences state it. A closure an island calls keeps the code it found
//! by its site, as an ordinary call keeps it (FIG-6844).

use super::embedder::{Embedder, Setup};
use crate::compile::expr::{self, Expr};
use crate::compile::islands::{self, Kind};
use crate::compile::{Event, Head, LoopPlan, Lowered, Refused, RegionSlot, Step};
use crate::costs::Costs;

const LOOP: &str = "definition \"function count(n: Any) -> Any\\nkernel 1\\nmetering statements\\nbody {  let i = 0 let s = 0 while num.lt(i, n) { set s = num.add(s, i) set i = num.add(i, 1) } return s  }\"\n\nmain { let r = call count(5) finish r }";

const WALK: &str = "definition \"function walk(xs: Any) -> Any\\nkernel 1\\nmetering statements\\nbody {  let s = 0 for x in xs { let y = num.add(x, 1) if num.lt(x, 3) { continue } else { set s = num.add(s, y) } } return s  }\"\n\nmain { let r = call walk([1, 2, 3, 4]) finish r }";

/// The one plan `text` forms, and what it is checked against.
fn checked(text: &str, check: impl FnOnce(&LoopPlan, &dyn Fn(&LoopPlan) -> Result<(), Refused>)) {
    checked_with(text, Setup::default(), check);
}

fn checked_with(
    text: &str,
    setup: Setup,
    check: impl FnOnce(&LoopPlan, &dyn Fn(&LoopPlan) -> Result<(), Refused>),
) {
    let flat = setup.probes.is_some();
    let (program, _) = Embedder::with(text, setup).program(true);
    let admitted = program
        .library
        .admit(&program.document, Default::default(), &program.selection)
        .unwrap_or_else(|_| panic!("the loop program is admitted"));
    let exe = admitted
        .executable(
            &program.document,
            &program.library,
            &program.documents,
            &program.functions,
            &program.instances,
        )
        .unwrap_or_else(|_| panic!("the loop program compiles"));
    let plans: Vec<_> = exe.islands.scalar.iter().collect();
    let [plan] = &plans[..] else {
        panic!("one verified loop plan forms, not {}", plans.len());
    };
    let (stream, _, _) = exe.statement(plan.stmt);
    let loop_test = Costs::of_document(&program.document).loop_test;
    let block = |id| exe.block(id);
    let stmt = |id| exe.stmt(id);
    let lib = |id| exe.lib(id);
    let lib_of = |function: &_| exe.library_lib(function);
    let island = |code| exe.island_of(code).is_some();
    let lowered = Lowered {
        stream,
        block: &block,
        stmt: &stmt,
        lib: &lib,
        lib_of: &lib_of,
        island: &island,
    };
    assert_eq!(plan.verify(&lowered, loop_test), Ok(()));
    if flat {
        plan.assert_flat_verified(&exe);
    }
    // Another cost table's loop test is not this plan's.
    assert_eq!(plan.verify(&lowered, loop_test + 1), Err(Refused::LoopTest));
    check(plan, &|plan| plan.verify(&lowered, loop_test));
}

/// `plan` with `change` made to a copy of it.
fn misstated(plan: &LoopPlan, change: impl FnOnce(&mut LoopPlan)) -> LoopPlan {
    let mut misstated = plan.copied();
    change(&mut misstated);
    misstated
}

#[test]
fn lowering_admits_a_loop_plan_only_as_its_stream_states_it() {
    checked(LOOP, |plan, verify| {
        let refused = |change: &dyn Fn(&mut LoopPlan)| verify(&misstated(plan, change));
        assert_eq!(
            refused(&|plan| plan.source_site.path.push(0)),
            Err(Refused::Room(crate::compile::room::Refused::Source))
        );
        // Summary `at`, numbered as the verifier numbers them: the test 0,
        // then the body's statements.
        let edit = |plan: &mut LoopPlan, at: usize, f: &dyn Fn(&mut crate::compile::Summary)| match (
            at,
            &mut plan.head,
            &mut plan.body.steps[..],
        ) {
            (0, Head::While(test), _) => f(test),
            (at, _, steps) => match &mut steps[at - 1] {
                Step::Write(summary) => f(summary),
                _ => panic!("the loop's body writes"),
            },
        };
        assert_eq!(
            refused(&|plan| edit(plan, 1, &|summary| summary.fixed += 1)),
            Err(Refused::Fixed { statement: 1 })
        );
        assert_eq!(
            refused(&|plan| edit(plan, 0, &|summary| summary.fixed -= 1)),
            Err(Refused::Fixed { statement: 0 })
        );
        assert_eq!(
            refused(&|plan| edit(plan, 2, &|summary| {
                let mut program = summary.program.to_vec();
                program.retain(|event| *event != Event::Formula);
                summary.program = program.into();
            })),
            Err(Refused::Program { statement: 2 })
        );
        assert_eq!(
            refused(&|plan| plan.iteration_max += 1),
            Err(Refused::Iteration)
        );
        assert_eq!(
            refused(&|plan| edit(plan, 1, &|summary| summary.write =
                summary.write.map(|slot| match slot {
                    RegionSlot::Frame(at) => RegionSlot::Frame(at + 1),
                    RegionSlot::Session(at) => RegionSlot::Session(at + 1),
                }))),
            Err(Refused::Slots { statement: 1 })
        );
        assert_eq!(
            refused(&|plan| plan.calls = vec![0].into()),
            Err(Refused::Calls)
        );
        // A scalar statement stated as allocating, and a scalar plan
        // stated as one that admits each statement (§5, R5).
        assert_eq!(
            refused(&|plan| edit(plan, 1, &|summary| summary.general = true)),
            Err(Refused::Allocates { statement: 1 })
        );
        assert_eq!(
            refused(&|plan| plan.allocates = true),
            Err(Refused::Allocates { statement: 0 })
        );
        assert_eq!(
            refused(&|plan| edit(plan, 0, &|summary| summary.program =
                summary.program[1..].into())),
            Err(Refused::Program { statement: 0 })
        );
    });
}

#[test]
fn lowering_admits_a_loop_body_tree_only_as_its_blocks_state_it() {
    let source = super::layout::scalar_document(
        "let i = 0 let s = 0 while flat.lt(i, 5) { if flat.lt(i, 2) { set s = flat.add(s, flat.mul(i, i)) } else { set s = flat.add(s, i) } set i = flat.add(i, 1) } return s",
    );
    checked_with(
        &source,
        Setup {
            probes: Some(super::layout::scalar_probes),
            ..Setup::default()
        },
        |_, _| {},
    );
    checked(WALK, |plan, verify| {
        let refused = |change: &dyn Fn(&mut LoopPlan)| verify(&misstated(plan, change));
        let arms = |plan: &mut LoopPlan, f: &dyn Fn(&mut Step, &mut Step)| {
            let Some(Step::If {
                then, otherwise, ..
            }) = plan.body.steps.get_mut(1)
            else {
                panic!("the body's second statement is an `if`");
            };
            let (Some(then), Some(otherwise)) =
                (then.steps.first_mut(), otherwise.steps.first_mut())
            else {
                panic!("both arms have a statement");
            };
            f(then, otherwise);
        };
        // The arms swapped are not the `if`'s.
        assert_eq!(
            refused(&|plan| arms(plan, &|then, otherwise| std::mem::swap(then, otherwise))),
            Err(Refused::Shape { statement: 3 })
        );
        // A `continue` the region would leave to the reference.
        assert_eq!(
            refused(&|plan| arms(plan, &|then, _| *then = Step::Leave)),
            Err(Refused::Shape { statement: 3 })
        );
        // The body's declarations, whose variables end with it.
        assert_eq!(
            refused(&|plan| plan.body.declares = Box::default()),
            Err(Refused::Shape { statement: 1 })
        );
        // A `let` stated as an assignment.
        assert_eq!(
            refused(&|plan| {
                if let Step::Bind(summary) = &plan.body.steps[0] {
                    plan.body.steps[0] = Step::Write(summary.clone());
                }
            }),
            Err(Refused::Shape { statement: 1 })
        );
        // The `for`'s head: another variable, and one unit more.
        assert_eq!(
            refused(&|plan| {
                if let Head::For { binding, .. } = &mut plan.head {
                    *binding += 1;
                }
            }),
            Err(Refused::Shape { statement: 0 })
        );
        assert_eq!(
            refused(&|plan| {
                if let Head::For { charge, .. } = &mut plan.head {
                    charge.fixed += 1;
                }
            }),
            Err(Refused::Fixed { statement: 0 })
        );
        assert_eq!(
            refused(&|plan| plan.binds = false),
            Err(Refused::Shape { statement: 0 })
        );
    });
}

const SESSIONS: &str = "main { let s = 0 let i = 0 \
                        while num.lt(i, 5) { set s = num.add(s, i) set i = num.add(i, 1) } finish s }";

#[test]
fn lowering_admits_a_session_plan_only_as_its_ops_name_the_bindings() {
    checked(SESSIONS, |plan, verify| {
        let refused = |change: &dyn Fn(&mut LoopPlan)| verify(&misstated(plan, change));
        // The test reads `i` first, so it is the plan's first binding.
        let named: Vec<_> = plan.sessions.iter().map(|name| name.as_str()).collect();
        assert_eq!(named, ["i", "s"]);
        assert_eq!(
            refused(&|plan| plan.sessions.reverse()),
            Err(Refused::Slots { statement: 0 })
        );
        assert_eq!(
            refused(&|plan| plan.sessions = plan.sessions[..1].into()),
            Err(Refused::Slots { statement: 0 })
        );
        // The body's first write stated as another binding's.
        assert_eq!(
            refused(&|plan| {
                if let Step::Write(summary) = &mut plan.body.steps[0] {
                    summary.write = Some(RegionSlot::Session(0));
                }
            }),
            Err(Refused::Slots { statement: 1 })
        );
    });
}

const ISLAND: &str = "definition \"function fib(self: Any, n: Any) -> Any\\nkernel 1\\nmetering statements\\nbody {  if num.lt(n, 2) { return n } return num.add(apply self(self, num.add(n, -1)), apply self(self, num.add(n, -2)))  }\"\n\nmain { let r = call fib(&fib, 5) finish r }";

#[test]
fn lowering_admits_an_island_plan_only_as_its_stream_states_it() {
    let (program, _) = Embedder::new(ISLAND).program(true);
    let admitted = program
        .library
        .admit(&program.document, Default::default(), &program.selection)
        .unwrap_or_else(|_| panic!("the island program is admitted"));
    let exe = admitted
        .executable(
            &program.document,
            &program.library,
            &program.documents,
            &program.functions,
            &program.instances,
        )
        .unwrap_or_else(|_| panic!("the island program compiles"));
    // Uncharged library bodies form register programs of their own; `fib`
    // forms exactly one.
    let fib = super::embedder::function_unit("fib");
    let fibs: Vec<_> = exe
        .islands
        .plans
        .iter()
        .filter(|plan| exe.code(plan.code).site.unit == fib)
        .collect();
    let [plan] = &fibs[..] else {
        panic!("one island of `fib` forms, not {}", fibs.len());
    };
    let (stream, _, _) = exe.statement(plan.body.statements[0].id);
    assert_eq!(plan.verify(stream), Ok(()));
    let refused = |change: &dyn Fn(&mut islands::IslandPlan)| {
        let mut misstated = plan.copied();
        change(&mut misstated);
        misstated.verify(stream)
    };
    let region = test_close(&mut plan.copied()).region;
    assert_eq!(
        refused(&|plan| test_close(plan).total += 1),
        Err(islands::Refused::Region { region })
    );
    assert_eq!(
        refused(&|plan| test_close(plan).head += 1),
        Err(islands::Refused::Region { region })
    );
    assert_eq!(
        refused(&|plan| plan.calls = plan.calls[1..].into()),
        Err(islands::Refused::Calls)
    );
    assert_eq!(
        refused(&|plan| plan.calls.reverse()),
        Err(islands::Refused::Calls)
    );
}

/// The region the island's `if` test closes at its operator.
fn test_close(plan: &mut islands::IslandPlan) -> &mut expr::Close {
    match &mut plan.body.statements[0].kind {
        Kind::If {
            cond:
                Expr {
                    kind: expr::Kind::Operator { closes, .. },
                    ..
                },
            ..
        } if closes.len() == 1 => &mut closes[0],
        _ => panic!("the island's test closes a region at its operator"),
    }
}

/// All source projections are inspected through the same code/specialization
/// owners used by prepared, typed, island, native/body and scalar entries.
fn source_rooms(check: impl Fn(&crate::compile::room::Source)) {
    use crate::compile::{CodeId, LibId};
    let typed_count = std::cell::Cell::new(0);
    let rejected_joins = std::cell::Cell::new(0);
    let typed_text = LOOP
        .replace("num.add", "law.typed_add")
        .replace("num.lt", "law.typed_less");
    for text in [
        typed_text.as_str(),
        LOOP,
        WALK,
        ISLAND,
        "main { let x = pair.twice(3) let ys = let(y = x, if(true, [y, if(false, x, y)], if(false, [x], [y]))) return text.join(ys, \"-\") }",
    ] {
        let (program, _) = Embedder::with(
            text,
            Setup {
                probes: Some(room_join),
                ..Setup::default()
            },
        )
        .program(true);
        let mut canonical = std::collections::BTreeMap::new();
        let mut holding_sites = std::collections::BTreeMap::new();
        for layout in [0, 1, 3, 17] {
            let program = super::embedder::program(
                std::sync::Arc::clone(&program.document),
                crate::PreparedLibrary::with_layout(
                    std::sync::Arc::clone(program.library.registry()),
                    crate::Layout(layout),
                ),
            );
            let admitted = program
                .library
                .admit(&program.document, Default::default(), &program.selection)
                .unwrap_or_else(|_| panic!("room corpus admits"));
            let exe = admitted
                .executable(
                    &program.document,
                    &program.library,
                    &program.documents,
                    &program.functions,
                    &program.instances,
                )
                .unwrap_or_else(|_| panic!("room corpus compiles"));
            for index in 0..exe.code_count() {
                assert!(
                    !exe.code(CodeId(index as u32)).source_room.is_derived(),
                    "compilation must not derive source memory programs on the worker path"
                );
            }
            exe.verify_stream().unwrap();
            for index in 0..exe.code_count() {
                let code = exe.code(CodeId(index as u32));
                code.source_room.verify().unwrap();
                let key = (code.source_room.owner, code.source_room.site.clone());
                let mut sites = std::collections::BTreeMap::new();
                let mut joins = std::collections::BTreeMap::new();
                let mut blocks = vec![code.body];
                while let Some(block) = blocks.pop() {
                    for stmt in &exe.block(block).stmts {
                        let (stream, stmt, _) = exe.statement(*stmt);
                        let mut ranges = vec![stmt.ops];
                        if let crate::compile::Form::While { test, .. } = stmt.form {
                            ranges.push(test);
                        }
                        for range in ranges {
                            if stream
                                .source_joins
                                .range(range.start..range.end())
                                .next()
                                .is_some()
                                && matches!(
                                    stream.ops[range.end() as usize - 1],
                                    lash_kernel_exec::Op::Bind { .. }
                                        | lash_kernel_exec::Op::BindSession { .. }
                                        | lash_kernel_exec::Op::Return { .. }
                                )
                            {
                                let mut omitted = crate::compile::Stream {
                                    ops: stream.ops.clone(),
                                    source_sites: stream.source_sites.clone(),
                                    source_joins: stream.source_joins.clone(),
                                    ..Default::default()
                                };
                                omitted
                                    .source_joins
                                    .retain(|pc, _| *pc < range.start || *pc >= range.end());
                                assert!(!omitted.verify_holding_sites(
                                    &code.source_room,
                                    range,
                                    stmt.regs
                                ));
                                rejected_joins.set(rejected_joins.get() + 1);
                            }
                            for pc in range.start..range.end() {
                                let op = stream.ops[pc as usize];
                                if matches!(op, lash_kernel_exec::Op::Region { .. }) {
                                    assert!(stream.source_sites[pc as usize].is_none());
                                    continue;
                                }
                                let site = stream.source_sites[pc as usize].as_ref().unwrap();
                                let kind = match op {
                                    lash_kernel_exec::Op::Operator { .. } => {
                                        lash_kernel_exec::OpKind::Call
                                    }
                                    _ => op.kind(),
                                };
                                *sites.entry((site.as_ref().clone(), kind)).or_insert(0) += 1;
                                if let Some(transfers) = stream.source_joins.get(&pc) {
                                    for transfer in transfers {
                                        *joins
                                            .entry((
                                                transfer.from.as_ref().clone(),
                                                transfer.to.as_ref().clone(),
                                            ))
                                            .or_insert(0) += 1;
                                    }
                                }
                            }
                        }
                        blocks.extend(stmt.form.blocks());
                    }
                }
                let holding_key = (key.clone(), code.capture_names.clone());
                let events = (sites, joins);
                if let Some(previous) = holding_sites.insert(holding_key, events.clone()) {
                    assert_eq!(previous, events, "source events survive physical layouts");
                }
                if let Some(previous) = canonical.get(&key) {
                    assert_eq!(
                        previous, &code.source_room.room,
                        "source owners survive physical layouts"
                    );
                } else {
                    canonical.insert(key, code.source_room.room.clone());
                }
                check(&code.source_room);
            }
            let mut body_native = false;
            let mut prepared = false;
            let mut join = false;
            for index in 0..exe.lib_count() {
                let lib = exe.lib(LibId(index as u32));
                if let Some(source) = lib.source_room(&exe) {
                    source.verify().unwrap();
                    assert_eq!(
                        source.owner,
                        lash_kernel_doc::constant_pool::PoolOwner::Function(
                            lib.specialization.function
                        )
                    );
                    body_native |=
                        lib.header.implementation == lash_kernel_doc::ImplementationKind::Both;
                }
                prepared |= lib.specialization.prepared.is_some();
                join |= lib.specialization.native.text_join;
            }
            assert!(
                (prepared || layout % 2 == 1) && body_native && join,
                "all entry declarations are in the inventory"
            );
            for plan in &exe.islands.plans {
                plan.verify(plan.stream(&exe)).unwrap();
                assert_eq!(plan.source_room.site, exe.code(plan.code).site);
                if let Some(compiled) = plan.compiled(&exe) {
                    typed_count
                        .set(typed_count.get() + compiled.verify_room_projections(&exe).unwrap());
                    assert!(std::sync::Arc::ptr_eq(
                        &compiled.source_room,
                        &plan.source_room
                    ));
                }
            }
        }
    }
    assert!(
        typed_count.get() > 0,
        "typed repeats retain their checked source prefix"
    );
    assert!(
        rejected_joins.get() > 0,
        "missing source ownership transfers are refused"
    );
}
fn room_join(
    registry: &mut lash_kernel_doc::FunctionRegistry,
    _: &std::collections::BTreeMap<&'static str, lash_kernel_value::FunctionId>,
) -> Vec<(&'static str, lash_kernel_value::FunctionId)> {
    let definition=lash_kernel_doc::parse_definition("function text.join(xs: Any, separator: Any) -> Text\nkernel 1\ncharge sum(1, size(result))\nnative\n").unwrap();
    let native = lash_kernel_lib::native("text.join").expect("real joined-text specialization");
    let mut out = vec![(
        "text.join",
        registry.register(definition, Some(native)).unwrap(),
    )];
    for (name, implementation, result) in [
        ("law.typed_add", "num.add", "Any"),
        ("law.typed_less", "num.lt", "Bool"),
    ] {
        let definition = lash_kernel_doc::parse_definition(&format!(
            "function {name}(a: Any, b: Any) -> {result}\nkernel 1\ncharge 1\nnative\n"
        ))
        .unwrap();
        out.push((
            name,
            registry
                .register(definition, lash_kernel_lib::native(implementation))
                .unwrap(),
        ));
    }
    out
}

#[test]
fn source_room_peak_bounds_follow_source_lifetimes() {
    use crate::compile::room::{Balance, Phase, Refused};
    room_shared_binding_growth();
    room_unpooled_immutable_growth();
    let source = room_numeric_source();
    let mut execution = RoomExecution::default();
    assert_eq!(
        source.evaluate(&mut execution, 100).unwrap().balance(),
        Balance {
            peak_extra: 160,
            retained_delta: 16
        }
    );
    assert_eq!(
        execution.tests, 2,
        "both source tests execute, including exhaustion"
    );
    let mut prefix = RoomExecution::default();
    assert_eq!(
        source
            .evaluate_prefix(&source.site.child(1), &mut prefix, 100)
            .unwrap()
            .balance(),
        Balance {
            peak_extra: 80,
            retained_delta: 0
        }
    );
    room_member_growth_peak();
    // Independent prices: frame 64, keep binding 16, operand 16, and
    // list construction's 32-byte entity + 16-byte reference + 16-byte member.
    assert!(execution.visited.iter().any(|p| p.phase == Phase::Leave));
    source_rooms(|source| {
        for point in source.room.peak_extra.keys() {
            let mut understated = source.room.clone();
            understated.peak_extra.get_mut(point).unwrap().peak.fixed -= 1;
            assert_eq!(source.verify_room(&understated), Err(Refused::Peak));
        }
    });
}

fn room_shared_binding_growth() {
    use crate::compile::room::{Phase, Point, Source};
    use lash_kernel_value::Name;
    use lash_kernel_value::sites::{Site, Unit};

    let document = lash_kernel_doc::parse_document(
        "numbers by_spelling\nkernel 1\nmain { let x = null return null }",
    )
    .unwrap();
    let root = Site::new(Unit::Main, vec![]);
    let declared = root.child(0);
    let parameter = Name::new("parameter");
    let source = Source::compile(
        lash_kernel_doc::constant_pool::PoolOwner::Document(document.identity()),
        root.clone(),
        document.main.as_ref().unwrap(),
        std::slice::from_ref(&parameter),
        std::collections::BTreeMap::from([
            ((root.clone(), parameter.clone()), true),
            ((declared.clone(), Name::new("x")), true),
        ]),
        &std::collections::BTreeMap::new(),
        &std::collections::BTreeSet::from([Name::new("x")]),
    );
    source.verify().unwrap();
    // The input occurrence moves into the cell. Its body is 32 bytes;
    // the new binding's separate reference is 16 bytes.
    for (site, phase, expected) in [
        (root, Phase::Enter, 64 + 32 + 16),
        (declared, Phase::Commit, 32 + 16),
    ] {
        let point = Point { site, phase };
        let event = &source.room.peak_extra[&point];
        assert_eq!(event.peak.fixed, expected, "{point:?}");
        assert_eq!(event.delta.fixed, expected, "{point:?}");
    }
}

fn room_unpooled_immutable_growth() {
    use crate::compile::room::{
        Balance, CallSource, CheckedRoom, Execution, Facts, Measure, Owner, Point, Source,
    };
    use lash_kernel_value::{
        FunctionId,
        sites::{Site, Unit},
    };
    struct Values(bool);
    impl Facts for Values {
        fn measure(&self, measure: &Measure) -> Option<i128> {
            match measure {
                Measure::Local(Owner::Operand(site)) => Some(if !self.0 {
                    16
                } else if site.path == [0, 0] {
                    16 + 2 * (16 + 4)
                } else {
                    16 + 4
                }),
                Measure::Release(owners) => owners.iter().try_fold(0, |sum, owner| {
                    Some(
                        sum + match owner {
                            Owner::Operand(_) => self.measure(&Measure::Local(owner.clone()))?,
                            _ => 0,
                        },
                    )
                }),
                _ => None,
            }
        }
        fn call(&self, _: &Site, _: CallSource) -> Option<&CheckedRoom> {
            None
        }
        fn raised(&self) -> bool {
            false
        }
        fn resolved(&self, _: &Site) -> Option<FunctionId> {
            None
        }
    }
    impl Execution for Values {
        fn event(&mut self, _: &Point) -> Option<Box<dyn Facts + '_>> {
            Some(Box::new(Values(self.0)))
        }
        fn choose(&mut self, _: &Site) -> Option<bool> {
            None
        }
    }
    let document = lash_kernel_doc::parse_document(
        "numbers by_spelling\nkernel 1\nmain { return (\"abcd\", \"efgh\") }",
    )
    .unwrap();
    let source = Source::compile(
        lash_kernel_doc::constant_pool::PoolOwner::Document(document.identity()),
        Site::new(Unit::Main, vec![]),
        document.main.as_ref().unwrap(),
        &[],
        Default::default(),
        &std::collections::BTreeMap::new(),
        &Default::default(),
    );
    source.verify().unwrap();
    for (owned, peak_extra, retained_delta) in [(false, 112, 16), (true, 120, 56)] {
        assert_eq!(
            source.evaluate(&mut Values(owned), 32).unwrap().balance(),
            Balance {
                peak_extra,
                retained_delta
            },
            "ordinary immutable values must retain their payload, owned={owned}"
        );
    }
}

#[test]
fn source_room_retained_delta_has_source_owners() {
    room_finally_departure();
    use crate::compile::room::Refused;
    source_rooms(|source| {
        for point in source.room.retained_delta.keys() {
            let mut understated = source.room.clone();
            understated
                .retained_delta
                .get_mut(point)
                .unwrap()
                .delta
                .fixed -= 1;
            assert_eq!(source.verify_room(&understated), Err(Refused::Retained));
        }
        for (point, event) in &source.room.peak_extra {
            if !event.entities.is_empty() {
                let mut unowned = source.room.clone();
                unowned.peak_extra.get_mut(point).unwrap().entities = Box::new([]);
                unowned.retained_delta.get_mut(point).unwrap().entities = Box::new([]);
                assert_eq!(source.verify_room(&unowned), Err(Refused::Retained));
            }
        }
    });
}

#[test]
fn source_room_scratch_ends_on_both_primitive_exits() {
    use crate::compile::room::{Phase, Refused};
    let count = std::cell::Cell::new(0);
    // Source contracts are checked even for scalar natives with zero logical
    // scratch. Absence of the native contract is still an eligibility miss.
    source_rooms(|source| {
        for index in 0..source.room.scratch.len() {
            count.set(count.get() + 1);
            let mut understated = source.room.clone();
            understated.scratch[index].raised.phase = Phase::Enter;
            assert_eq!(source.verify_room(&understated), Err(Refused::Scratch));
            understated = source.room.clone();
            understated.scratch[index].normal.phase = Phase::Leave;
            assert_eq!(source.verify_room(&understated), Err(Refused::Scratch));
            understated = source.room.clone();
            understated.scratch[index].amount.fixed -= 1;
            assert_eq!(source.verify_room(&understated), Err(Refused::Scratch));
        }
    });
    assert!(
        count.get() > 0,
        "native primitive scratch obligations were inspected"
    );
    room_native_envelope();
}

#[test]
fn source_room_exits_preserve_charge_prefix_and_holdings() {
    use crate::compile::room::{Phase, Refused};
    source_rooms(|source| {
        for index in 0..source.room.exits.len() {
            use crate::compile::room::Owner;
            assert!(source.room.exits[index].owners.iter().all(|o| matches!(
                o,
                Owner::Frame(_)
                    | Owner::Scope(_)
                    | Owner::Operands(_)
                    | Owner::Sessions(_)
                    | Owner::Departures(_)
            )));
            let mut understated = source.room.clone();
            understated.exits[index].owners = Box::new([]);
            assert_eq!(source.verify_room(&understated), Err(Refused::Exit));
            understated = source.room.clone();
            let prefix = &mut understated.exits[index].charge_prefix;
            prefix.phase = if prefix.phase == Phase::Enter {
                Phase::Leave
            } else {
                Phase::Enter
            };
            assert_eq!(source.verify_room(&understated), Err(Refused::Exit));
        }
    });
}

fn room_numeric_source() -> crate::compile::room::Source {
    use lash_kernel_value::sites::{Site, Unit};
    let document=lash_kernel_doc::parse_document("numbers by_spelling\nkernel 1\nmain { let keep = true while keep { set keep = false do [null] } return null }").unwrap();
    crate::compile::room::Source::compile(
        lash_kernel_doc::constant_pool::PoolOwner::Document(document.identity()),
        Site::new(Unit::Main, vec![]),
        document.main.as_ref().unwrap(),
        &[],
        Default::default(),
        &std::collections::BTreeMap::new(),
        &std::collections::BTreeSet::from([lash_kernel_value::Name::new("keep")]),
    )
}
#[derive(Default)]
struct RoomExecution {
    tests: usize,
    visited: Vec<crate::compile::room::Point>,
}
impl crate::compile::room::Execution for RoomExecution {
    fn event(
        &mut self,
        point: &crate::compile::room::Point,
    ) -> Option<Box<dyn crate::compile::room::Facts + '_>> {
        self.visited.push(point.clone());
        Some(Box::new(RoomFacts))
    }
    fn choose(&mut self, _: &lash_kernel_value::sites::Site) -> Option<bool> {
        self.tests += 1;
        Some(self.tests == 1)
    }
}
struct RoomFacts;
impl crate::compile::room::Facts for RoomFacts {
    fn measure(&self, m: &crate::compile::room::Measure) -> Option<i128> {
        use crate::compile::room::{Measure, Owner};
        match m {
            Measure::Local(_) => Some(16),
            Measure::Members(_) => Some(16),
            Measure::Release(owners) => Some(
                owners
                    .iter()
                    .map(|o| match o {
                        Owner::Scope(site) if site.path.is_empty() => 16,
                        Owner::Visible { .. } => 16,
                        Owner::Operand(site) if site.path == [1, 1, 1, 0] => 64,
                        Owner::Operand(_) => 16,
                        _ => 0,
                    })
                    .sum(),
            ),
            _ => None,
        }
    }
    fn call(
        &self,
        _: &lash_kernel_value::sites::Site,
        _: crate::compile::room::CallSource,
    ) -> Option<&crate::compile::room::CheckedRoom> {
        None
    }
    fn raised(&self) -> bool {
        false
    }
    fn resolved(
        &self,
        _: &lash_kernel_value::sites::Site,
    ) -> Option<lash_kernel_value::FunctionId> {
        None
    }
}

fn room_native_envelope() {
    use crate::compile::room::{Balance, CheckedRoom, Refused};
    use lash_kernel_doc::{
        NativeResourceContract as Contract, NativeResourceEntity as Entity,
        NativeResourceEvent as Event, NativeResourceExit as Exit, NativeResourceHolding as Holding,
        NativeResourceOrigin as Origin, NativeResourceOwner as Owner,
        NativeResourceRelease as Release,
    };
    let definition = lash_kernel_doc::parse_definition(
        "function law.room() -> Any\nkernel 1\ncharge 1\nnative\n",
    )
    .unwrap();
    let function = definition.identity();
    let source = room_native_source(definition);
    for exit in [Exit::Returned, Exit::Raised] {
        let contract = Contract {
            exit,
            output: Holding {
                bytes: 16,
                origin: Origin::Immutable,
            },
            events: vec![
                Event::Entity {
                    owner: Owner(0),
                    kind: Entity::List,
                    release: Some(Release::Exit),
                    members: vec![Holding {
                        bytes: 16,
                        origin: Origin::Immutable,
                    }],
                },
                Event::Scratch {
                    owner: Owner(1),
                    bytes: 64,
                    release: Release::Exit,
                },
            ],
        };
        let checked = CheckedRoom::native(function, &contract).unwrap();
        // Temporary list 32 + member 16, scratch 64, output 16. The
        // discarded list must contribute to peak, including on a raised exit.
        assert_eq!(
            checked.balance(),
            Balance {
                peak_extra: 128,
                retained_delta: 16
            }
        );
        let mut execution = RoomNativeExecution(&checked);
        assert_eq!(
            source.evaluate(&mut execution, 20).unwrap().balance(),
            Balance {
                peak_extra: 192,
                retained_delta: 16
            }
        );
        let other = lash_kernel_doc::parse_definition(
            "function law.room() -> Any\nkernel 1\ncharge 2\nnative\n",
        )
        .unwrap();
        assert!(matches!(
            room_native_source(other).evaluate(&mut execution, 20),
            Err(Refused::Scratch | Refused::Facts)
        ));
        let body = lash_kernel_doc::parse_definition(
            "function law.room() -> Any\nkernel 1\ncharge 1\nnative\nbody { return null }\n",
        )
        .unwrap();
        let unchecked_body = CheckedRoom::native(body.identity(), &contract).unwrap();
        let body_source = crate::compile::room::Source::compile(
            lash_kernel_doc::constant_pool::PoolOwner::Function(body.identity()),
            lash_kernel_value::sites::Site::new(
                lash_kernel_value::sites::Unit::Library(body.identity()),
                vec![],
            ),
            &body.body().unwrap().block,
            &[],
            Default::default(),
            &std::collections::BTreeMap::new(),
            &Default::default(),
        );
        let whole = body_source
            .evaluate(&mut RoomNativeExecution(&unchecked_body), 20)
            .unwrap();
        let fragment = body_source
            .evaluate_prefix(
                &body_source.site.child(0).child(0),
                &mut RoomNativeExecution(&unchecked_body),
                20,
            )
            .unwrap();
        let caller = room_native_source(body.clone());
        assert!(matches!(
            caller.evaluate(&mut RoomNativeExecution(&fragment), 20),
            Err(Refused::Facts)
        ));
        assert_eq!(
            caller
                .evaluate(&mut RoomNativeExecution(&whole), 20)
                .unwrap()
                .balance(),
            Balance {
                peak_extra: 144,
                retained_delta: 16
            }
        );

        assert!(matches!(
            room_native_source(body).evaluate(&mut RoomNativeExecution(&unchecked_body), 20),
            Err(Refused::Facts)
        ));
    }
}
fn room_native_source(
    definition: lash_kernel_doc::FunctionDefinition,
) -> crate::compile::room::Source {
    use lash_kernel_value::sites::{Site, Unit};
    let function = definition.identity();
    let mut document =
        lash_kernel_doc::parse_document("numbers by_spelling\nkernel 1\nmain { return null }")
            .unwrap();
    document.main = Some(vec![lash_kernel_doc::Stmt::Return {
        value: lash_kernel_doc::Expr::Call {
            function,
            args: vec![],
        },
    }]);
    let catalog = std::collections::BTreeMap::from([(function, definition)]);
    crate::compile::room::Source::compile(
        lash_kernel_doc::constant_pool::PoolOwner::Document(document.identity()),
        Site::new(Unit::Main, vec![]),
        document.main.as_ref().unwrap(),
        &[],
        Default::default(),
        &catalog,
        &Default::default(),
    )
}
struct RoomNativeExecution<'a>(&'a crate::compile::room::CheckedRoom);
impl crate::compile::room::Execution for RoomNativeExecution<'_> {
    fn event(
        &mut self,
        _: &crate::compile::room::Point,
    ) -> Option<Box<dyn crate::compile::room::Facts + '_>> {
        Some(Box::new(RoomNativeFacts(self.0)))
    }
    fn choose(&mut self, _: &lash_kernel_value::sites::Site) -> Option<bool> {
        None
    }
}
struct RoomNativeFacts<'a>(&'a crate::compile::room::CheckedRoom);
impl crate::compile::room::Facts for RoomNativeFacts<'_> {
    fn measure(&self, m: &crate::compile::room::Measure) -> Option<i128> {
        matches!(m, crate::compile::room::Measure::Release(_)).then_some(0)
    }
    fn call(
        &self,
        _: &lash_kernel_value::sites::Site,
        _: crate::compile::room::CallSource,
    ) -> Option<&crate::compile::room::CheckedRoom> {
        Some(self.0)
    }
    fn raised(&self) -> bool {
        false
    }
    fn resolved(
        &self,
        _: &lash_kernel_value::sites::Site,
    ) -> Option<lash_kernel_value::FunctionId> {
        None
    }
}

fn room_member_growth_peak() {
    use crate::compile::room::{
        CallSource, CheckedRoom, Execution, Facts, Measure, Owner, Point, Source,
    };
    use lash_kernel_value::{
        FunctionId,
        sites::{Site, Unit},
    };
    struct Trace;
    struct Values;
    impl Execution for Trace {
        fn event(&mut self, _: &Point) -> Option<Box<dyn Facts + '_>> {
            Some(Box::new(Values))
        }
        fn choose(&mut self, _: &Site) -> Option<bool> {
            None
        }
    }
    impl Facts for Values {
        fn measure(&self, m: &Measure) -> Option<i128> {
            match m {
                Measure::MemberDelta(_) => Some(516),
                Measure::Release(owners) if owners.iter().any(|o| matches!(o, Owner::Scope(_))) => {
                    Some(0)
                }
                _ => RoomFacts.measure(m),
            }
        }
        fn call(&self, _: &Site, _: CallSource) -> Option<&CheckedRoom> {
            None
        }
        fn raised(&self) -> bool {
            false
        }
        fn resolved(&self, _: &Site) -> Option<FunctionId> {
            None
        }
    }
    let field = "k".repeat(500);
    let document = lash_kernel_doc::parse_document(&format!(
        "numbers by_spelling\nkernel 1\nmain {{ set r.{field} = null return null }}"
    ))
    .unwrap();
    let source = Source::compile(
        lash_kernel_doc::constant_pool::PoolOwner::Document(document.identity()),
        Site::new(Unit::Main, vec![]),
        document.main.as_ref().unwrap(),
        &[],
        Default::default(),
        &std::collections::BTreeMap::new(),
        &Default::default(),
    );
    let balance = source
        .evaluate_prefix(&source.site.child(0), &mut Trace, 20)
        .unwrap()
        .balance();
    // The new field owns 500 name bytes and one 16-byte value. During
    // publication the receiver's evaluated 16-byte occurrence is still held.
    assert!(
        balance.peak_extra >= 532,
        "member growth cannot be hidden by operand release: {balance:?}"
    );
    assert_eq!(balance.retained_delta, 516);
}

fn room_finally_departure() {
    use crate::compile::room::{
        CallSource, CheckedRoom, Execution, Facts, Measure, Owner, Point, Source,
    };
    use lash_kernel_value::{
        FunctionId,
        sites::{Site, Unit},
    };
    struct Trace;
    struct Values;
    impl Execution for Trace {
        fn event(&mut self, _: &Point) -> Option<Box<dyn Facts + '_>> {
            Some(Box::new(Values))
        }
        fn choose(&mut self, _: &Site) -> Option<bool> {
            None
        }
    }
    impl Facts for Values {
        fn measure(&self, m: &Measure) -> Option<i128> {
            match m {
                Measure::Local(_) => Some(16),
                Measure::Release(owners) => Some(
                    owners
                        .iter()
                        .map(|o| match o {
                            Owner::Operand(_) | Owner::PendingDeparture(_) => 16,
                            _ => 0,
                        })
                        .sum(),
                ),
                _ => None,
            }
        }
        fn call(&self, _: &Site, _: CallSource) -> Option<&CheckedRoom> {
            None
        }
        fn raised(&self) -> bool {
            false
        }
        fn resolved(&self, _: &Site) -> Option<FunctionId> {
            None
        }
    }
    let document = lash_kernel_doc::parse_document(
        "numbers by_spelling\nkernel 1\nmain { try { return \"old\" } finally { return null } }",
    )
    .unwrap();
    let source = Source::compile(
        lash_kernel_doc::constant_pool::PoolOwner::Document(document.identity()),
        Site::new(Unit::Main, vec![]),
        document.main.as_ref().unwrap(),
        &[],
        Default::default(),
        &std::collections::BTreeMap::new(),
        &Default::default(),
    );
    assert_eq!(
        source
            .evaluate(&mut Trace, 100)
            .unwrap()
            .balance()
            .retained_delta,
        16,
        "a replacing finally releases the suspended return occurrence"
    );
}
