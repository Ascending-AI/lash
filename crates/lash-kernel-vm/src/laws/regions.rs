//! The loop region verifier (`docs/design/kernel-regions.md` §1): lowering
//! admits a plan only as its stream's charge regions and statement units
//! state it, and refuses each misstatement of a summary.

use super::embedder::Embedder;
use crate::compile::{Event, Refused};
use crate::costs::Costs;

const LOOP: &str = "fn count(n) { let i = 0 let s = 0 \
                    while num.lt(i, n) { set s = num.add(s, i) set i = num.add(i, 1) } return s }\n\
                    main { let r = call count(5) finish r }";

#[test]
fn lowering_admits_a_loop_plan_only_as_its_stream_states_it() {
    let (program, _) = Embedder::new(LOOP).program(true);
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
    let (stream, statement, _) = exe.statement(plan.stmt);
    let loop_test = Costs::of_document(&program.document).loop_test;
    assert_eq!(plan.verify(stream, statement, loop_test), Ok(()));
    let refused = |change: &dyn Fn(&mut crate::compile::LoopPlan)| {
        let mut misstated = plan.copied();
        change(&mut misstated);
        misstated.verify(stream, statement, loop_test)
    };
    assert_eq!(
        refused(&|plan| plan.statements[0].fixed += 1),
        Err(Refused::Fixed { statement: 1 })
    );
    assert_eq!(
        refused(&|plan| plan.test.fixed -= 1),
        Err(Refused::Fixed { statement: 0 })
    );
    assert_eq!(
        refused(&|plan| {
            let mut program = plan.statements[1].program.to_vec();
            program.retain(|event| *event != Event::Formula);
            plan.statements[1].program = program.into();
        }),
        Err(Refused::Program { statement: 2 })
    );
    assert_eq!(
        refused(&|plan| plan.iteration_max += 1),
        Err(Refused::Iteration)
    );
    assert_eq!(
        refused(&|plan| plan.statements[0].write = plan.statements[1].write),
        Err(Refused::Slots { statement: 1 })
    );
    assert_eq!(
        refused(&|plan| plan.calls = vec![0].into()),
        Err(Refused::Calls)
    );
    assert_eq!(
        refused(&|plan| plan.test.program = plan.test.program[1..].into()),
        Err(Refused::Program { statement: 0 })
    );
    // Another cost table's loop test is not this plan's.
    assert_eq!(
        plan.verify(stream, statement, loop_test + 1),
        Err(Refused::LoopTest)
    );
}
