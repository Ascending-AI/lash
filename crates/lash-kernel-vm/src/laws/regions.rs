//! The loop region verifier (`docs/design/kernel-regions.md` §1): lowering
//! admits a plan only as its lowered code, its stream's charge regions and
//! its statement units state it, and refuses each misstatement of a
//! summary, of the body's tree and of a `for`'s head.

use super::embedder::Embedder;
use crate::compile::{Event, Head, LoopPlan, Lowered, Refused, Step};
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
    let lowered = Lowered {
        stream,
        block: &block,
        stmt: &stmt,
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
                summary.write.map(|at| at + 1))),
            Err(Refused::Slots { statement: 1 })
        );
        assert_eq!(
            refused(&|plan| plan.calls = vec![0].into()),
            Err(Refused::Calls)
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
