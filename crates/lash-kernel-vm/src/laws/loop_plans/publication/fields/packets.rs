//! K-EVAL-008/K-CHG-001: real packet member prefixes preserve source exits.
use super::*;
use crate::machine::islands::member_region as runtime;
use lash_kernel_doc as doc;

pub(super) fn prefixes() {
    cold_and_limits();
    for (body, private) in [
        (
            "let values = [1,1,1] set values[0] = 2 set values[1] = 258 set values[0] = 259 return values",
            true,
        ),
        (
            "let values = {left:1,rght:1} set values.left = 2 set values.rght = 258 set values.left = 259 return values",
            true,
        ),
        (
            "let values = map{0:1,1:1,2:9} set values[0] = values[2] set values[1.0] = values[0] set values[0] = 258 return values",
            true,
        ),
        (
            "let child = [9] set child[1] = child let values = [1,1,child] set values[0] = values[2] set values[1] = values[0] set values[0] = 258 return values[0]",
            true,
        ),
        (
            "let n = 1 let first = fn() { return n } let second = fn() { return n } set n = flat.add(n, 1) set n = flat.add(n, 256) set n = flat.add(n, 1) let a = apply first() let b = apply second() return (a,b)",
            true,
        ),
        (
            "let n = 1 let run = fn() { set n = flat.add(n, 1) set n = flat.add(n, 256) set n = flat.add(n, 1) return n } let a = apply run() let b = apply run() return (a,b,n)",
            false,
        ),
    ] {
        let source = crate::laws::layout::scalar_document(body);
        for record in [false, true] {
            runtime::take_publications();
            crate::laws::layout::packet_modes(
                &source,
                crate::laws::embedder::ROOMY,
                u64::MAX,
                false,
                record,
            );
            let (composed, prefixes) = runtime::take_publications();
            if private {
                assert!(
                    composed > 0,
                    "real packet entry must publish a composed member prefix: {body}"
                );
            } else {
                assert_eq!(
                    (composed, prefixes),
                    (0, 0),
                    "a saved caller still observes the captured cell; this must stay an exact handoff"
                );
            }
            assert!(prefixes >= composed);
            eprintln!(
                "member packet publications: {composed} composed/{prefixes} total; tape={record}"
            );
        }
    }
}

type SourceStop = (
    &'static str,
    crate::Meters,
    [u64; 3],
    Option<lash_kernel_state::semantic::Checkpoint>,
);

#[derive(Debug, PartialEq)]
struct Observation {
    end: String,
    printed: Vec<doc::Datum>,
    requests: Vec<crate::Request>,
    tape: Vec<lash_kernel_exec::Weight>,
    charged: u64,
    stops: Vec<SourceStop>,
}
#[derive(Default)]
struct Publications {
    composed: u64,
    reopened: u64,
}
fn observe(
    text: &str,
    layout: crate::Layout,
    record: bool,
    cold: bool,
    bounds: crate::Bounds,
    mode: crate::laws::layout::ExecutionMode,
    slice: u64,
) -> (Observation, Publications) {
    use crate::laws::layout::ExecutionMode;
    use crate::{End, Machine, Step};
    let mut run = Embedder::with(
        text,
        Setup {
            bounds,
            layout,
            slice,
            probes: Some(crate::laws::layout::scalar_probes),
            ..Setup::default()
        },
    );
    let tape = crate::ChargeTape::new();
    if record {
        run.machine.record_charges(tape.clone());
    }
    let mut stops = Vec::new();
    let mut published = Publications::default();
    let mut reopened = false;
    let end = loop {
        match mode {
            ExecutionMode::Source => run.machine.force_source_execution(),
            ExecutionMode::Generic => run.machine.force_generic_packets(),
            ExecutionMode::Specialized => {}
        }
        runtime::take_publications();
        let step = run.run();
        let (composed, all) = runtime::take_publications();
        if mode != ExecutionMode::Specialized {
            assert_eq!((composed, all), (0, 0));
        }
        published.composed += composed;
        if reopened {
            published.reopened += composed;
        }
        assert!(run.machine.execution_storage_released());
        let kind = match &step {
            Step::Ended(_) => "ended",
            Step::Slice => "slice",
            Step::Parked(_) => "parked",
        };
        let image = (!matches!(&step, Step::Ended(_))).then(|| run.machine.semantic_checkpoint());
        let balance = run.machine.logical_balance().unwrap();
        stops.push((
            kind,
            run.machine.meters(),
            [
                balance.entity_local_bytes,
                balance.root_and_control_bytes,
                balance.entities,
            ],
            image,
        ));
        match step {
            Step::Ended(end) => {
                break match end {
                    End::Finished(done) => format!("finished {:?}", done.result),
                    other => format!("{other:?}"),
                };
            }
            Step::Slice => {
                if cold {
                    run.reopen(layout, true);
                    reopened = true;
                    if record {
                        run.machine.record_charges(tape.clone());
                    }
                }
            }
            Step::Parked(_) => {
                if cold {
                    run.reopen(layout, true);
                    reopened = true;
                    if record {
                        run.machine.record_charges(tape.clone());
                    }
                }
                run.deliver_next();
            }
        }
    };
    let events = tape
        .steps()
        .into_iter()
        .flat_map(|weight| match weight {
            lash_kernel_exec::Weight::Units(units) => {
                vec![lash_kernel_exec::Weight::Units(1); units as usize]
            }
            other => vec![other],
        })
        .collect();
    (
        Observation {
            end,
            printed: run.world.printed,
            requests: run.requests,
            tape: events,
            charged: run.machine.meters().charged.unwrap(),
            stops,
        },
        published,
    )
}

/// A parked source frame reopens with empty caches between two private
/// prefixes. Both captured aliases must still read the installed final cell.
fn cold_and_limits() {
    use crate::Layout;
    use crate::laws::layout::ExecutionMode as Mode;
    let source = crate::laws::layout::scalar_document(
        "let n = 1 let first = fn() { return n } let second = fn() { return n } set n = flat.add(n, 1) set n = flat.add(n, 256) set n = flat.add(n, 1) do perform 0(n) as Any set n = flat.add(n, 1) set n = flat.add(n, 256) set n = flat.add(n, 1) let a = apply first() let b = apply second() return (a,b)",
    );
    let roomy = crate::laws::embedder::ROOMY;
    for layout in [Layout(0), Layout(2), Layout(42)] {
        for record in [false, true] {
            for (slice, cold) in [(u64::MAX, false), (u64::MAX, true), (47, true), (1, true)] {
                let (expected, _) =
                    observe(&source, layout, record, cold, roomy, Mode::Source, slice);
                let (generic, _) =
                    observe(&source, layout, record, cold, roomy, Mode::Generic, slice);
                assert_eq!(
                    generic, expected,
                    "generic cold member continuation {layout:?}, tape={record}, slice={slice}"
                );
                let (actual, published) = observe(
                    &source,
                    layout,
                    record,
                    cold,
                    roomy,
                    Mode::Specialized,
                    slice,
                );
                assert_eq!(
                    actual, expected,
                    "private cold member continuation {layout:?}, tape={record}, slice={slice}"
                );
                if slice == u64::MAX {
                    assert!(
                        published.composed >= 2,
                        "both real source prefixes must publish"
                    );
                    if cold {
                        assert!(
                            published.reopened > 0,
                            "a composed prefix must actually publish after cold reopen"
                        );
                    }
                }
                eprintln!(
                    "member continuation publications: {} composed/{} after reopen; {layout:?}, tape={record}, slice={slice}",
                    published.composed, published.reopened
                );
            }
        }
    }
    let (reference, _) = observe(
        &source,
        Layout(0),
        true,
        false,
        roomy,
        Mode::Source,
        u64::MAX,
    );
    for charge_limit in 0..=reference.charged {
        let bounds = crate::Bounds {
            charge_limit,
            ..roomy
        };
        let (expected, _) = observe(
            &source,
            Layout(0),
            true,
            false,
            bounds,
            Mode::Source,
            u64::MAX,
        );
        let (generic, _) = observe(
            &source,
            Layout(0),
            true,
            false,
            bounds,
            Mode::Generic,
            u64::MAX,
        );
        let (actual, _) = observe(
            &source,
            Layout(0),
            true,
            false,
            bounds,
            Mode::Specialized,
            u64::MAX,
        );
        assert_eq!(
            generic, expected,
            "generic member charge edge {charge_limit}"
        );
        assert_eq!(
            actual, expected,
            "private member charge edge {charge_limit}"
        );
    }
    for memory in [1, 64, 128, 256, 512, 1024] {
        let bounds = crate::Bounds { memory, ..roomy };
        let (expected, _) = observe(
            &source,
            Layout(0),
            true,
            false,
            bounds,
            Mode::Source,
            u64::MAX,
        );
        let (generic, _) = observe(
            &source,
            Layout(0),
            true,
            false,
            bounds,
            Mode::Generic,
            u64::MAX,
        );
        let (actual, _) = observe(
            &source,
            Layout(0),
            true,
            false,
            bounds,
            Mode::Specialized,
            u64::MAX,
        );
        assert_eq!(generic, expected, "generic member memory edge {memory}");
        assert_eq!(actual, expected, "private member memory edge {memory}");
    }
}
