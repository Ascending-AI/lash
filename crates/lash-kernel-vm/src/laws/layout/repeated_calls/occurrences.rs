//! K-EFF-008: the last occurrence admits once; the next iteration stays unpaid.
use super::*;
use crate::machine::islands::compiled::{
    repeat_laws, take_loop_invocations, take_loop_publications,
};

fn force(run: &mut Embedder, mode: ExecutionMode) {
    match mode {
        ExecutionMode::Source => run.machine.force_source_execution(),
        ExecutionMode::Generic => run.machine.force_generic_packets(),
        ExecutionMode::Specialized => {}
    }
}

pub(super) fn runtime() {
    started();
    let helper = super::super::super::embedder::fragment(
        "function advance(a: Any, b: Any) -> Any\nkernel 1\nmetering statements\nbody { if flat.lt(a, b) { return flat.add(a, b) } return flat.add(a, b) }",
    );
    for body in [
        "let i = 0 let s = 0 while flat.lt(i,4) { set s = flat.add(s,flat.mul(i,i)) set i = invoke advance(i,1) } return (s,i)",
        "let i = 0 let s = 0 while flat.lt(i,4) { set s = flat.add(s,invoke advance(i,i)) set i = invoke advance(i,1) } return (s,i)",
    ] {
        let source = format!("definition \"{helper}\"\n{}", scalar_document(body));
        for initial in [u64::MAX - 1, u64::MAX] {
            for layout in [Layout(0), Layout(2), Layout(0x5eed)] {
                let mut reference = None;
                for mode in [
                    ExecutionMode::Source,
                    ExecutionMode::Generic,
                    ExecutionMode::Specialized,
                ] {
                    let mut run = Embedder::with(
                        &source,
                        Setup {
                            layout,
                            probes: Some(scalar_probes),
                            ..Setup::default()
                        },
                    );
                    assert!(repeat_laws::seed_actions(&mut run.machine, initial) > 0);
                    let before = repeat_laws::action_counts(&mut run.machine);
                    let first_invocation = repeat_laws::invocation_cursor(&run.machine);
                    let tape = crate::ChargeTape::new();
                    run.machine.record_charges(tape.clone());
                    take_loop_publications();
                    take_loop_invocations();
                    crate::machine::islands::REPEATED_TEMPLATE.with(|n| n.set(0));
                    crate::machine::islands::witness::begin();
                    force(&mut run, mode);
                    let result = run.try_run();
                    assert!(
                        matches!(result, Err(crate::MachineError::Fault { ref problem })
                        if problem == "occurrence counter overflow"),
                        "{result:?}"
                    );
                    let witness = crate::machine::islands::witness::take();
                    let after = repeat_laws::action_counts(&mut run.machine);
                    assert!(
                        before
                            .iter()
                            .zip(&after)
                            .all(|(before, after)| *before != initial || *after == u64::MAX),
                        "{body} {mode:?} {layout:?} initial={initial} before={before:?} after={after:?}"
                    );
                    let published = take_loop_publications();
                    let invocations = take_loop_invocations();
                    let observation = (
                        result,
                        run.machine.meters(),
                        lash_kernel_exec::Weight::expand(tape.steps()),
                        after,
                        run.machine.logical_balance().unwrap(),
                        repeat_laws::invocation_cursor(&run.machine) - first_invocation,
                    );
                    if let Some(reference) = &reference {
                        assert_eq!(&observation, reference, "{layout:?} {mode:?}");
                    } else {
                        reference = Some(observation);
                    }
                    // Odd layouts intentionally disable native fast paths.
                    if mode == ExecutionMode::Specialized && layout.0.is_multiple_of(2) {
                        assert_eq!(
                            published,
                            u64::from(initial != u64::MAX),
                            "{body} {mode:?} {layout:?} initial={initial} {witness:?}"
                        );
                        assert_eq!(
                            crate::machine::islands::REPEATED_TEMPLATE.with(|n| n.get()),
                            u64::from(initial != u64::MAX)
                        );
                        assert_eq!(
                            invocations,
                            if initial == u64::MAX {
                                0
                            } else {
                                before.iter().filter(|count| **count == initial).count() as u64
                            }
                        );
                    }
                }
            }
        }
    }
}

fn started() {
    // The real saved Loop.started counter also bounds a batch before staging.
    let source = scalar_document("let i = 0 while flat.lt(i,4) { set i = flat.add(i,1) } return i");
    for mode in [
        ExecutionMode::Source,
        ExecutionMode::Generic,
        ExecutionMode::Specialized,
    ] {
        let mut run = Embedder::with(
            &source,
            Setup {
                slice: 1,
                probes: Some(scalar_probes),
                ..Setup::default()
            },
        );
        let mut seeded = false;
        for _ in 0..1000 {
            force(&mut run, mode);
            assert!(matches!(run.run(), Step::Slice));
            if repeat_laws::seed_started(&mut run.machine, u64::MAX - 1) {
                seeded = true;
                break;
            }
        }
        assert!(seeded, "the actual while header must be reachable");
        take_loop_publications();
        crate::machine::islands::REPEATED_TEMPLATE.with(|n| n.set(0));
        force(&mut run, mode);
        let result = run.machine.run(&mut run.world, u64::MAX);
        assert!(
            matches!(result, Err(crate::MachineError::Fault { ref problem })
            if problem == "loop occurrence overflow"),
            "{result:?}"
        );
        assert_eq!(
            take_loop_publications(),
            u64::from(mode == ExecutionMode::Specialized),
            "the admitted final prefix publishes"
        );
        assert_eq!(
            crate::machine::islands::REPEATED_TEMPLATE.with(|n| n.get()),
            u64::from(mode == ExecutionMode::Specialized)
        );
    }
}
