//! K-MACH-002 / K-MACH-008: a helper's native removes its interior slice
//! points from the whole image, including its Decline body (7bcba3681b).

use super::*;
use lash_kernel_vm::{End, KernelMachine};
use std::sync::Arc;

fn observe(
    program: &Program,
    slice: u64,
    cold: bool,
) -> (crate::ExpectedEnd, Vec<Datum>, Vec<(String, u64)>) {
    let bounds = Bounds {
        memory_meter: lash_kernel_vm::MemoryMeter::On,
        charge: lash_kernel_vm::Charge::Exact,
        charge_limit: 50_000_000,
        memory: 64 << 20,
        call_depth: Bounds::DEFAULT_CALL_DEPTH,
        live_tasks: 100,
        requests_per_park: 100,
        join_members: 100,
    };
    let source = crate::pages::Source::default();
    let mut machine = KernelMachine::start(
        program.clone(),
        bounds,
        CellStart {
            owner: crate::pages::OWNER,
            target: Target::Main,
            args: Vec::new(),
            session: None,
            modes: Default::default(),
            host: Vec::new(),
        },
        Box::new(source.clone()),
    )
    .unwrap();
    let mut world = World::default();
    let mut stops = Vec::new();
    loop {
        let step = machine.run(&mut world, slice).unwrap();
        stops.push((
            match &step {
                Step::Slice => "slice",
                Step::Parked(_) => "parked",
                Step::Ended(_) => "ended",
            }
            .into(),
            machine.meters().charged.unwrap(),
        ));
        if let Step::Ended(end) = step {
            assert!(
                matches!(end, End::Finished(_)),
                "numeric fixture must finish: {end:?}"
            );
            return (crate::ExpectedEnd::from_end(end), world.prints, stops);
        }
        assert!(
            matches!(step, Step::Slice),
            "numeric fixture has no effects"
        );
        if cold {
            let sealed = machine.checkpoint().unwrap();
            machine = KernelMachine::open(
                program.clone(),
                bounds,
                &sealed.checkpoint(),
                Box::new(source.clone()),
            )
            .unwrap();
        }
    }
}

#[test]
fn numeric_natives_and_decline_bodies_stop_at_the_same_places_at_every_slice_size() {
    for (dialect, names, pairs) in [
        (
            "python",
            &[
                "py.add",
                "py.number.iadd",
                "py.number.floordiv",
                "py.number.truediv",
                "py.number.mod",
            ][..],
            &["7, 3", "-7, 3", "7.5, 3.0", "9223372036854775807, 1"][..],
        ),
        (
            "typescript",
            &[
                "ts.mul", "ts.div", "ts.rem", "ts.lt", "ts.le", "ts.gt", "ts.ge",
            ][..],
            &["7.0, 3.0", "\"7\", \"3\"", "nan, -0.0"][..],
        ),
    ] {
        let runner = runner::Runner::new(dialect).unwrap();
        let case = runner::Case {
            id: dialect.into(),
            dialect: dialect.into(),
            source: if dialect == "python" { "None" } else { "null;" }.into(),
            bits: 0,
            stride: 0,
            nanoseconds: 0,
        };
        let runner::Lowered::Program(base) = runner.lower(&case).unwrap() else {
            panic!("numeric fixture lowers")
        };
        let alternatives = crate::declining_bodies(&base.library);
        assert!(
            Arc::ptr_eq(base.library.helpers(), alternatives.helpers()),
            "both paths use one immutable helper image"
        );
        for name in names {
            let (function, registered) = base
                .library
                .registry()
                .iter()
                .find(|(_, registered)| registered.definition().name.as_str() == *name)
                .unwrap();
            assert!(registered.native().is_some(), "{name} exercises a native");
            assert!(
                alternatives
                    .registry()
                    .get(function)
                    .unwrap()
                    .native()
                    .is_some(),
                "{name} retains the image's native classification"
            );
            for pair in pairs {
                let source = format!(
                    "kernel 1\nnumbers by_spelling\nmain {{ let answer = invoke @{function}({pair}) print answer return answer }}"
                );
                let mut document = lash_kernel_doc::parse_document(&source).unwrap();
                document.library.insert(*function);
                let native = crate::linked::program(Arc::new(document), base.library.clone());
                let body = Program {
                    library: alternatives.clone(),
                    ..native.clone()
                };
                let reference = observe(&native, u64::MAX, false);
                let charged = reference.2.last().unwrap().1;
                assert!(charged > 0, "the exhaustive slice range is nonempty");
                for slice in 1..=charged + 1 {
                    let expected = observe(&native, slice, false);
                    assert_eq!(expected.0, reference.0, "{name}({pair}) slice {slice} end");
                    assert_eq!(
                        expected.1, reference.1,
                        "{name}({pair}) slice {slice} prints"
                    );
                    for cold in [false, true] {
                        assert_eq!(
                            observe(&body, slice, cold),
                            expected,
                            "{name}({pair}) body slice {slice}, cold={cold}"
                        );
                    }
                    assert_eq!(
                        observe(&native, slice, true),
                        expected,
                        "{name}({pair}) native slice {slice}, cold=true"
                    );
                }
            }
        }
    }
}

/// PLAN-INDEP / K-CHG-002 / K-MACH-008: a traced comparison starts on
/// floats, then declines on a cold heap text whose numeric conversion is
/// the ordinary helper's. Both paths retain every completion and stop.
#[test]
fn guarded_comparison_loops_keep_cold_fallback_stops() {
    let runner = runner::Runner::new("typescript").unwrap();
    let case = runner::Case {
        id: "cold-comparison-loop".into(),
        dialect: "typescript".into(),
        source: "null;".into(),
        bits: 0,
        stride: 0,
        nanoseconds: 0,
    };
    let runner::Lowered::Program(base) = runner.lower(&case).unwrap() else {
        panic!("comparison fixture lowers")
    };
    let function = |name| {
        *base
            .library
            .registry()
            .iter()
            .find(|(_, registered)| registered.definition().name.as_str() == name)
            .unwrap()
            .0
    };
    let less = function("ts.lt");
    let add = function("ts.add");
    // One byte beyond inline capacity, so reopening must read a heap text.
    let bound = format!("{}3", "0".repeat(lash_kernel_doc::arena::ShortText::MAX));
    let source = format!(
        r#"kernel 1
numbers by_spelling
main {{
  let bound = 3.0
  let i = 0.0
  while invoke @{less}(i, bound) {{
    print i
    set bound = "{bound}"
    set i = invoke @{add}(i, 1.0)
  }}
  return i
}}"#
    );
    let mut document = lash_kernel_doc::parse_document(&source).unwrap();
    document.library.insert(less);
    document.library.insert(add);
    let alternatives = crate::declining_bodies(&base.library);
    assert!(Arc::ptr_eq(base.library.helpers(), alternatives.helpers()));
    let native = crate::linked::program(Arc::new(document), base.library.clone());
    let body = Program {
        library: alternatives,
        ..native.clone()
    };
    let reference = observe(&native, u64::MAX, false);
    assert_eq!(reference.1.len(), 3, "the heap-operand fallback must run");
    let charged = reference.2.last().unwrap().1;
    assert!(charged > 0);
    for slice in 1..=charged + 1 {
        let expected = observe(&native, slice, false);
        for cold in [false, true] {
            assert_eq!(
                observe(&native, slice, cold),
                expected,
                "native slice {slice}, cold {cold}"
            );
            assert_eq!(
                observe(&body, slice, cold),
                expected,
                "body slice {slice}, cold {cold}"
            );
        }
    }
}
