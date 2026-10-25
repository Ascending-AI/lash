//! The region verifiers (`docs/design/kernel-regions.md` §1): lowering
//! admits a loop plan only as its lowered code, its stream's charge regions
//! and its statement units state it, and refuses each misstatement of a
//! summary, of the body's tree and of a `for`'s head; and it admits an
//! island plan only as its stream's charge regions and occurrences state it.
//! A closure an island calls keeps the code it found by its site, as the
//! reference's call keeps it (FIG-6844). A loop region's guest calls
//! resolve their callees once while nothing is freed, and again after a
//! collection (FIG-6982).

use super::embedder::{Embedder, Setup, int, result, run};
use crate::compile::expr::{self, Expr};
use crate::compile::islands::{self, Kind};
use crate::compile::{Event, Head, LoopPlan, Lowered, Refused, RegionSlot, Step};
use crate::costs::Costs;
use crate::paged::Machine as _;

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
    let plans: Vec<_> = exe.regions.iter().chain(&exe.islands.scalar).collect();
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

/// `COLLECTED` with each pass leaving `garbage`: a statement of the loop's
/// body, before its guest call.
fn collected(garbage: &str) -> String {
    format!(
        "definition \"function count(n: Any) -> Any\\nkernel 1\\nmetering statements\\nbody {{  let echo = fn(v) {{ return v }} let tag = fn(v) {{ do apply echo(v) return 1 }} let i = 0 let s = 0 let t = [0] while num.lt(i, n) {{ {garbage_json} set s = num.add(s, apply tag(i)) set i = num.add(i, 1) }} return s  }}\"\n\nmain {{ let r = call count(3000) finish r }}",
        garbage_json = crate::laws::embedder::fragment(garbage)
    )
}

/// The closures islands resolved, the scopes regions' guest calls opened,
/// and the callees those calls resolved, on this thread so far.
fn resolutions() -> [u64; 3] {
    use crate::machine::islands::{GUESTS, RESOLVED, SCOPES};
    [&RESOLVED, &SCOPES, &GUESTS].map(|count| count.with(std::cell::Cell::get))
}

/// What `run` resolved and opened while it ran.
fn counted<T>(run: impl FnOnce() -> T) -> (T, [u64; 3]) {
    let before = resolutions();
    let ran = run();
    let after = resolutions();
    (ran, [0, 1, 2].map(|at| after[at] - before[at]))
}

#[test]
fn a_collection_between_guest_calls_resolves_their_callees_again() {
    // Nothing freed: the region's 3000 guest calls of `tag`, and its
    // island's calls of `echo`, share one scope, each closure resolved
    // once in it.
    let ((end, _), [resolved, scopes, guests]) = counted(|| run(&collected("")));
    assert_eq!(result(end), int(3000));
    assert_eq!((resolved, scopes, guests), (2, 1, 1));
    // Each pass leaves a list for a release to free: a release comes due
    // between two guest calls and ends the batch at that statement
    // boundary. The next batch's first guest call opens a scope afresh,
    // and resolves `tag` again from the arena as the release left it;
    // later calls in the batch find it. The run ends, and charges, as
    // the reference's does.
    let text = collected("set t = [i]");
    let ((end, on), [_, scopes, guests]) = counted(|| run(&text));
    assert!(scopes > 1, "releases end the batch between guest calls");
    assert_eq!(
        guests, scopes,
        "each scope resolves the guest call's callee once"
    );
    let mut off = Embedder::with(
        &text,
        Setup {
            layout: crate::compile::Layout(0).without_regions(),
            ..Default::default()
        },
    );
    let reference = off.run_to_end(&[]);
    assert_eq!(on.machine.meters().charged, off.machine.meters().charged);
    assert_eq!(result(end), result(reference));
}
