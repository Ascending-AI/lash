//! A body-native number answer observes the ordinary call-depth bound.

use super::super::embedder::ROOMY;
use crate::{Bounds, Machine, Step};

/// K-BND-002: a body native's fused number answer needs the same operation
/// depth as its ordinary call, including when a trace reaches the bound.
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
            "function guarded.lt(a: Any, b: Any) -> Bool\nkernel 1\ncharge 1\nnative\nuse real.lt = @{}\nbody {{ return real.lt(a, b) }}",
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
    let observe = |depth, exact, slice| {
        let mut machine = super::super::embedder::core_machine(
            SOURCE,
            Bounds {
                call_depth: depth,
                ..ROOMY
            },
            Some(probes),
        )
        .unwrap();
        if exact {
            machine.record_charges(crate::ChargeTape::new());
        }
        let mut world = super::super::embedder::World::default();
        let mut slices = Vec::new();
        loop {
            match machine.run(&mut world, slice).unwrap() {
                Step::Ended(end) => break (format!("{end:?}"), machine.meters().charged, slices),
                Step::Slice => slices.push(machine.meters().charged),
                Step::Parked(_) => panic!("a comparison makes no request"),
            }
        }
    };
    let before = crate::vm::trace::ran();
    let whole = observe(ROOMY.call_depth, false, u64::MAX);
    assert!(whole.0.contains("Finished"), "{whole:?}");
    assert!(
        crate::vm::trace::ran() > before,
        "no guarded number trace ran"
    );
    for depth in 0..=4 {
        for slice in [1, 7, u64::MAX] {
            assert_eq!(
                observe(depth, false, slice),
                observe(depth, true, slice),
                "depth {depth}, slice {slice}"
            );
        }
    }
}
