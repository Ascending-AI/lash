//! A body-native number answer observes the ordinary call-depth bound.

use super::super::embedder::ROOMY;
use crate::{Bounds, Machine, Step};

/// K-BND-002: a body native's fused number answer needs the same operation
/// depth as its ordinary call, including when a trace reaches the bound.
/// K-CHG-001 / K-MACH-008: the comparison's float guard preserves the
/// first refusal and every slice, including cold and mixed operands.
#[test]
fn a_body_natives_fused_number_answer_preserves_the_depth_bound() {
    fn probes(
        registry: &mut lash_kernel_doc::FunctionRegistry,
        ids: &std::collections::BTreeMap<&'static str, lash_kernel_value::FunctionId>,
    ) -> Vec<(&'static str, lash_kernel_value::FunctionId)> {
        use lash_kernel_doc::arena::native::{InlineNative, InlineOperation, ScalarDomain};
        use lash_kernel_doc::scalar::{ScalarOp, floats};
        use lash_kernel_doc::{
            NativeCall, NativeError, NativeFunction, NativeSpecialization, Value,
        };
        struct Less;
        impl NativeFunction for Less {
            fn specialization(&self) -> NativeSpecialization {
                NativeSpecialization {
                    inline: Some(InlineNative {
                        native_version: lash_kernel_doc::FIRST_NATIVE_VERSION,
                        operation: InlineOperation::Scalar {
                            operation: ScalarOp::Lt,
                            domain: ScalarDomain::Floats,
                        },
                    }),
                    ..Default::default()
                }
            }
            fn call(&self, call: NativeCall<'_, '_>) -> Result<Value, NativeError> {
                match call.args.values() {
                    [Value::Float(a), Value::Float(b)] => {
                        floats(ScalarOp::Lt, *a, *b).ok_or(NativeError::Decline)
                    }
                    _ => Err(NativeError::Decline),
                }
            }
        }
        let definition = lash_kernel_doc::parse_definition(&format!(
            "function guarded.lt(a: Any, b: Any) -> Bool\nkernel 1\ncharge sum(3, size(a), size(b), size(result))\nnative\nuse real.lt = @{}\nbody {{ return real.lt(a, b) }}",
            ids["real.lt"]
        )).unwrap();
        vec![(
            "guarded.lt",
            registry
                .register(definition, Some(std::sync::Arc::new(Less)))
                .unwrap(),
        )]
    }
    const SOURCE: &str =
        "main { let j = 0.0 while guarded.lt(j, 3.0) { set j = real.add(j, 1.0) } return j }";
    let observe = |source: &str, bounds, exact, slice, cold| {
        let program = super::super::embedder::core_program(source, Some(probes));
        let pages = super::super::embedder::Source::default();
        let mut machine = crate::CoreMachine::start(
            program.clone(),
            bounds,
            super::super::embedder::Setup::default().start,
            Box::new(pages.clone()),
        )
        .unwrap();
        if exact {
            machine.record_charges(crate::ChargeTape::new());
        }
        let mut world = super::super::embedder::World::default();
        let mut slices = Vec::new();
        loop {
            match machine.run(&mut world, slice).unwrap() {
                Step::Ended(end) => {
                    break (
                        format!("{end:?}"),
                        machine.meters().charged,
                        slices,
                        world.printed,
                    );
                }
                Step::Slice => {
                    slices.push(machine.meters().charged);
                    if cold {
                        // A real cold continuation can enter traces, unlike
                        // the statement-by-statement forced-cold test mode.
                        let sealed = machine.checkpoint().unwrap();
                        machine = crate::CoreMachine::open(
                            program.clone(),
                            bounds,
                            &sealed.checkpoint(),
                            Box::new(pages.clone()),
                        )
                        .unwrap();
                        if exact {
                            machine.record_charges(crate::ChargeTape::new());
                        }
                    }
                }
                Step::Parked(_) => panic!("a comparison makes no request"),
            }
        }
    };
    let before = crate::vm::trace::ran();
    let whole = observe(SOURCE, ROOMY, false, u64::MAX, false);
    assert!(whole.0.contains("Finished"), "{whole:?}");
    assert!(
        crate::vm::trace::ran() > before,
        "no guarded number trace ran"
    );
    for depth in 0..=4 {
        for slice in [1, 7, u64::MAX] {
            let bounds = Bounds {
                call_depth: depth,
                ..ROOMY
            };
            assert_eq!(
                observe(SOURCE, bounds, false, slice, false),
                observe(SOURCE, bounds, true, slice, false),
                "depth {depth}, slice {slice}"
            );
        }
    }
    let mut memory_refused = false;
    for source in [
        SOURCE,
        // Signed zero and infinities keep binary64 comparison semantics.
        "main { let j = -0.0 while guarded.lt(j, 3.0) { print guarded.lt(j, inf) set j = real.add(j, 1.0) } return j }",
        // The same site changes from the proved representation to a mixed
        // pair, a NaN, or a non-number that raises in the ordinary body.
        "main { let bound = 3.0 let j = 0.0 while guarded.lt(j, bound) { set bound = 3 set j = real.add(j, 1.0) } return j }",
        "main { let bound = 3.0 let j = 0.0 while guarded.lt(j, bound) { set bound = nan set j = real.add(j, 1.0) } return j }",
        "main { let bound = 3.0 let j = 0.0 while guarded.lt(j, bound) { set bound = \"not numeric\" set j = real.add(j, 1.0) } return j }",
        // A long-text list stays live across the loop and a cold read.
        "main { let xs = [] let j = 0.0 while guarded.lt(j, 3.0) { set xs[list.len(xs)] = text.concat(\"long text held across a comparison\", \" and an allocation\") print xs[0] set j = real.add(j, 1.0) } return j }",
    ] {
        let total = observe(source, ROOMY, true, u64::MAX, false).1.unwrap();
        for cold in [false, true] {
            for slice in 1..=total + 1 {
                assert_eq!(
                    observe(source, ROOMY, false, slice, cold),
                    observe(source, ROOMY, true, slice, cold),
                    "slice {slice}, cold {cold}: {source}"
                );
            }
            for limit in 0..=total + 1 {
                let bounds = Bounds {
                    charge_limit: limit,
                    ..ROOMY
                };
                assert_eq!(
                    observe(source, bounds, false, 7, cold),
                    observe(source, bounds, true, 7, cold),
                    "limit {limit}, cold {cold}: {source}"
                );
            }
            for memory in (2_000..16_000).step_by(113) {
                let bounds = Bounds { memory, ..ROOMY };
                let expected = observe(source, bounds, true, 7, cold);
                memory_refused |= expected.0.contains("Memory") && expected.1.unwrap_or(0) > 0;
                assert_eq!(
                    observe(source, bounds, false, 7, cold),
                    expected,
                    "memory {memory}, cold {cold}: {source}"
                );
            }
        }
    }
    assert!(memory_refused, "no interior memory refusal");
}
