use std::sync::Arc;

use lash_kernel_vm::{Bounds, Program};

use super::machine::{BOUNDS, ExecutionObservation, execution_observation, installed, lower};

fn compare(program: &Program, bounds: Bounds, cold: bool) -> ExecutionObservation {
    let composed = execution_observation(program.clone(), bounds, cold);
    let ordinary = lash_kernel_vm::testing::without_lookup_words(|| {
        execution_observation(program.clone(), bounds, cold)
    });
    assert_eq!(composed, ordinary, "bounds {bounds:?}, cold {cold}");
    composed
}

/// The installed helper image must retain the constant-key lookup/update
/// proof when the numeric update has an installed native (FIG-7688).
#[test]
fn constant_key_reads_keep_their_word_update_under_the_current_helper_release() {
    let source = r#"def benchmark(n):
    obj = {"a":1,"b":2,"c":3,"d":4}
    result = 0
    for j in range(n):
        result += obj["a"]
        result += obj["b"]
        result += obj["c"]
        result += obj["d"]
    return result
print(benchmark(2))
"#;
    let lowered = lower(source).expect("benchmark source lowers");
    let code = lash_kernel_vm::testing::compile_document(
        &lowered.document,
        installed().prepared.registry(),
        lash_kernel_value::LinkId::from_bytes([0; 32]),
        None,
    )
    .expect("benchmark compiles");
    // Both the closure and the direct entry retain the source's four reads.
    assert_eq!(
        code.lookup_word_sites(),
        8,
        "constant-key Lookup with its word update was not derived under the current helper release; \
         check py.number.iadd: its resolved scalar declaration and completion formula must match the word update"
    );

    let schedule = r#"obj = {"a": 17}
result = 0
result += obj["a"]
alias = obj
alias["a"] = 0
result += obj["a"]
print(result)
"#;
    let lowered = lower(schedule).unwrap();
    let program = super::linked::program(
        Arc::clone(lowered.document.shared()),
        installed().prepared.clone(),
    );
    let whole = compare(&program, BOUNDS, false);
    assert!(matches!(whole.end, lash_kernel_vm::End::Finished(_)));
    assert_eq!(whole.lines, ["17"]);
    assert!(whole.charged > 0);
    for cold in [false, true] {
        for charge_limit in 0..=whole.charged {
            compare(
                &program,
                Bounds {
                    charge_limit,
                    ..BOUNDS
                },
                cold,
            );
        }
        for memory in [512, 1024, 2048, 4096, 8192, 16384, 32768, 65536] {
            compare(&program, Bounds { memory, ..BOUNDS }, cold);
        }
        for call_depth in [0, 1, 2, 3, 4, 8] {
            compare(
                &program,
                Bounds {
                    call_depth,
                    ..BOUNDS
                },
                cold,
            );
        }
    }
    let fallback = r#"obj = {"a": 1}
result = 9223372036854775807
result += obj["a"]
print(result)
obj["a"] = "wrong kind"
try:
    result += obj["a"]
except TypeError:
    print(0)
try:
    result += obj["missing"]
except KeyError:
    print(0)
"#;
    let lowered = lower(fallback).unwrap();
    let program = super::linked::program(
        Arc::clone(lowered.document.shared()),
        installed().prepared.clone(),
    );
    for cold in [false, true] {
        compare(&program, BOUNDS, cold);
    }
}
