//! The region verifiers (`docs/design/kernel-regions.md` §1): lowering
//! admits a loop plan only as its lowered code, its stream's charge regions
//! and its statement units state it, and refuses each misstatement of a
//! summary, of the body's tree and of a `for`'s head; and it admits an
//! island plan only as its stream's charge regions and occurrences state it.
//! A closure an island calls keeps the code it found by its site, as the
//! reference's call keeps it (FIG-6844).

use super::embedder::{Embedder, int, result, run};
use crate::compile::expr::{self, Expr};
use crate::compile::islands::{self, Kind};
use crate::compile::{Event, Head, LoopPlan, Lowered, Refused, RegionSlot, Step};
use crate::costs::Costs;

const LOOP: &str = "fn count(n) { let i = 0 let s = 0 \
                    while num.lt(i, n) { set s = num.add(s, i) set i = num.add(i, 1) } return s }\n\
                    main { let r = call count(5) finish r }";

const WALK: &str = "fn walk(xs) { let s = 0 \
                    for x in xs { let y = num.add(x, 1) if num.lt(x, 3) { continue } else { set s = num.add(s, y) } } return s }\n\
                    main { let r = call walk([1, 2, 3, 4]) finish r }";

/// The one plan `text` forms, and what it is checked against.
fn checked(text: &str, check: impl FnOnce(&LoopPlan, &dyn Fn(&LoopPlan) -> Result<(), Refused>)) {
    let (program, _) = Embedder::new(text).program(true);
    let admitted = program
        .library
        .admit(&program.document, Default::default())
        .unwrap_or_else(|_| panic!("the loop program is admitted"));
    let exe = admitted
        .executable(&program.document, &program.library)
        .unwrap_or_else(|_| panic!("the loop program compiles"));
    let [plan] = &exe.regions[..] else {
        panic!("one loop region forms, not {}", exe.regions.len());
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

const ISLAND: &str = "fn fib(n) { if num.lt(n, 2) { return n } \
                      return num.add(call fib(num.add(n, -1)), call fib(num.add(n, -2))) }\n\
                      main { let r = call fib(5) finish r }";

#[test]
fn lowering_admits_an_island_plan_only_as_its_stream_states_it() {
    let (program, _) = Embedder::new(ISLAND).program(true);
    let admitted = program
        .library
        .admit(&program.document, Default::default())
        .unwrap_or_else(|_| panic!("the island program is admitted"));
    let exe = admitted
        .executable(&program.document, &program.library)
        .unwrap_or_else(|_| panic!("the island program compiles"));
    let [plan] = &exe.islands.plans[..] else {
        panic!("one island forms, not {}", exe.islands.plans.len());
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

/// A loop region calling `tag` through its island, which calls `echo`
/// through its own call site's cache: neither closure is ever called by
/// the reference.
const CLOSURES: &str = "fn count(n) { let echo = fn(v) { return v } \
                        let tag = fn(v) { if num.lt(v, 0) { return 0 } do apply echo(v) return 1 } \
                        let i = 0 let s = 0 \
                        while num.lt(i, n) { set s = num.add(s, apply tag(num.add(i, -4))) set i = num.add(i, 1) } \
                        return s }\n\
                        main { let r = call count(12) finish r }";

#[test]
fn a_closure_an_island_calls_finds_its_code_by_its_site_once() {
    let projected = || crate::machine::project::PROJECTED.with(std::cell::Cell::get);
    let before = projected();
    let (end, _) = run(CLOSURES);
    assert_eq!(result(end), int(8));
    // `tag` and `echo` each find their code once, on their first call:
    // every later call, from the region and from inside the island, reads
    // the code the closure kept.
    assert_eq!(projected() - before, 2);
}
