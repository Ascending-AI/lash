//! K-EVAL-008 / K-BND-002: an installed Native-with-body remains a real call.
use super::*;
use lash_kernel_doc::{
    FunctionRegistry, NativeCall, NativeError, NativeFunction, PreparedNative, Value,
};
use std::{collections::BTreeMap, sync::Arc};

fn probes(
    registry: &mut FunctionRegistry,
    ids: &BTreeMap<&'static str, lash_kernel_value::names::FunctionId>,
) -> Vec<(&'static str, lash_kernel_value::names::FunctionId)> {
    struct Wrapped {
        native: Arc<dyn NativeFunction>,
        known: bool,
    }
    impl NativeFunction for Wrapped {
        fn call(&self, call: NativeCall<'_, '_>) -> Result<Value, NativeError> {
            self.native.call(call)
        }
        fn prepare(&self) -> Option<PreparedNative> {
            self.native.prepare()
        }
        fn resource_contract(
            &self,
            args: lash_kernel_doc::Args<'_>,
        ) -> Result<Option<lash_kernel_doc::NativeResourceContract>, NativeError> {
            if self.known {
                self.native.resource_contract(args)
            } else {
                Ok(None)
            }
        }
    }
    let mut functions = scalar_probes(registry, ids);
    let add = functions
        .iter()
        .find(|(name, _)| *name == "flat.add")
        .unwrap()
        .1;
    for (name, known, primitive) in [
        ("wrapped.known", true, add),
        ("wrapped.unknown", false, add),
        (
            "wrapped.lt",
            true,
            functions
                .iter()
                .find(|(name, _)| *name == "flat.lt")
                .unwrap()
                .1,
        ),
    ] {
        let implementation = Arc::clone(registry.get(&primitive).unwrap().native().unwrap());
        let operation = if name == "wrapped.lt" {
            "flat.lt"
        } else {
            "flat.add"
        };
        let definition = lash_kernel_doc::parse_definition(&format!(
            "function {name}(a: Any, b: Any) -> Any\nkernel 1\ncharge sum(7, size(a), size(b), size(result))\nuse {operation} = @{primitive}\nnative\nbody {{ return {operation}(a,b) }}"
        )).unwrap();
        functions.push((
            name,
            registry
                .register(
                    definition,
                    Some(Arc::new(Wrapped {
                        native: Arc::clone(&implementation),
                        known,
                    })),
                )
                .unwrap(),
        ));
    }
    functions
}
fn observe(
    source: &str,
    mode: ExecutionMode,
    layout: Layout,
    record: bool,
    slice: u64,
    cold: bool,
    bounds: Bounds,
) -> (ScalarObserved, ScalarStops) {
    let mut embedder = Embedder::with(
        source,
        Setup {
            probes: Some(probes),
            bounds,
            layout,
            slice,
            ..Setup::default()
        },
    );
    let tape = crate::ChargeTape::new();
    if record {
        embedder.machine.record_charges(tape.clone());
    }
    let mut stops = Vec::new();
    let end = loop {
        match mode {
            ExecutionMode::Source => embedder.machine.force_source_execution(),
            ExecutionMode::Generic => embedder.machine.force_generic_packets(),
            ExecutionMode::Specialized => {}
        }
        let step = embedder.run();
        assert!(embedder.machine.execution_storage_released());
        let balance = embedder.machine.logical_balance().unwrap();
        stops.push((
            if matches!(step, Step::Ended(_)) {
                "ended"
            } else {
                "slice"
            },
            embedder.machine.meters(),
            [
                balance.entity_local_bytes,
                balance.root_and_control_bytes,
                balance.entities,
            ],
            (!matches!(step, Step::Ended(_))).then(|| embedder.machine.semantic_checkpoint()),
        ));
        match step {
            Step::Ended(end) => {
                break match end {
                    End::Finished(value) => format!("finished {:?}", value.result),
                    other => format!("{other:?}"),
                };
            }
            Step::Slice => {
                if cold {
                    embedder.reopen(layout, true);
                    if record {
                        embedder.machine.record_charges(tape.clone());
                    }
                }
            }
            Step::Parked(_) => panic!("pure Native-with-body fixture parked"),
        }
    };
    let events = tape
        .steps()
        .into_iter()
        .flat_map(|weight| match weight {
            lash_kernel_exec::Weight::Units(n) => {
                vec![lash_kernel_exec::Weight::Units(1); n as usize]
            }
            other => vec![other],
        })
        .collect();
    (
        (
            end,
            embedder.world.printed,
            embedder.requests,
            events,
            embedder.machine.meters().charged.unwrap(),
            vec![],
        ),
        stops,
    )
}
pub(super) fn runtime() {
    use crate::machine::islands::compiled::{take_loop_invocations, take_loop_publications};
    for (name, eligible, test, invocations) in [
        ("wrapped.known", true, "wrapped.lt", 8),
        ("wrapped.known", true, "flat.lt", 4),
        ("wrapped.unknown", false, "flat.lt", 0),
    ] {
        let source = scalar_document(&format!(
            "let i = 0 let s = 0 while {test}(i,4) {{ set s = flat.add(s,flat.mul(i,i)) let previous = flat.identity(i) set i = invoke {name}(previous,1) }} return (s,i)"
        ));
        take_loop_publications();
        take_loop_invocations();
        let roomy = observe(
            &source,
            ExecutionMode::Specialized,
            Layout(0),
            true,
            u64::MAX,
            false,
            super::super::super::embedder::ROOMY,
        );
        assert_eq!(
            roomy.0.0,
            format!("finished {:?}", Datum::Tuple(vec![int(14), int(4)]))
        );
        assert_eq!(
            take_loop_publications(),
            u64::from(eligible),
            "installed native contract: {name}"
        );
        assert_eq!(
            take_loop_invocations(),
            invocations,
            "completed source invocations: {name}"
        );
        for layout in [Layout(0), Layout(1), Layout(2), Layout(u64::MAX)] {
            for record in [false, true] {
                for (slice, cold) in [(u64::MAX, false), (roomy.0.4 / 2, true), (3, true)] {
                    let run = |mode| {
                        observe(
                            &source,
                            mode,
                            layout,
                            record,
                            slice,
                            cold,
                            super::super::super::embedder::ROOMY,
                        )
                    };
                    let expected = run(ExecutionMode::Source);
                    assert_eq!(run(ExecutionMode::Generic), expected);
                    assert_eq!(
                        run(ExecutionMode::Specialized),
                        expected,
                        "native body {name}, {layout:?}, cold={cold}"
                    );
                }
            }
        }
    }
}
