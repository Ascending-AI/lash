//! K-EVAL-008: saved caller operands survive composed calls and cold cuts.
use super::*;
use crate::laws::embedder::ROOMY;

pub(super) fn entry(definition: &impl Fn(&str, &str) -> String) {
    let composed_source = format!(
        "{}{}main {{ return invoke composed_call() }}",
        definition(
            "remainder(a: Any, b: Any)",
            "let reduced = flat.rem(a, b) return reduced"
        ),
        definition(
            "composed_call()",
            "let answer = flat.add(5, invoke remainder(3, 4)) do perform 0(answer) as Any return answer"
        )
    );
    crate::machine::islands::take_composed_statements();
    let composed_reference = observe_source_or_register(
        &composed_source,
        Layout(0),
        u64::MAX,
        ROOMY,
        false,
        false,
        Mode::Source,
    );
    let composed_actual = observe_source_or_register(
        &composed_source,
        Layout(0),
        u64::MAX,
        ROOMY,
        false,
        false,
        Mode::Specialized,
    );
    assert_eq!(composed_actual, composed_reference);
    let composed = crate::machine::islands::take_composed_statements();
    assert!(
        composed > 0,
        "the saved caller operand and pure callee must execute before physical frame entry"
    );
    println!("composed caller/callee/continuation statements published: {composed}");
    for layout in [0, 1, 42, u64::MAX] {
        for taped in [false, true] {
            let reference = observe_source_or_register(
                &composed_source,
                Layout(layout),
                u64::MAX,
                ROOMY,
                taped,
                false,
                Mode::Source,
            );
            for mode in [Mode::Generic, Mode::Specialized] {
                assert_eq!(
                    observe_source_or_register(
                        &composed_source,
                        Layout(layout),
                        u64::MAX,
                        ROOMY,
                        taped,
                        false,
                        mode
                    ),
                    reference
                );
            }
            for slice in [1, 2, 3, 7] {
                let source = observe_source_or_register(
                    &composed_source,
                    Layout(layout),
                    slice,
                    ROOMY,
                    taped,
                    true,
                    Mode::Source,
                );
                assert_eq!(
                    observe_source_or_register(
                        &composed_source,
                        Layout(layout),
                        slice,
                        ROOMY,
                        taped,
                        true,
                        Mode::Specialized
                    ),
                    source
                );
            }
        }
    }
    // The complete envelope may decline; every tight prefix still agrees
    // on its first refusal, paid events, and retained source continuation.
    let total = composed_reference.stops.last().unwrap().1.charged.unwrap();
    for charge in 0..=total {
        let bounds = Bounds {
            charge_limit: charge,
            ..ROOMY
        };
        let reference = observe_source_or_register(
            &composed_source,
            Layout(0),
            u64::MAX,
            bounds,
            true,
            false,
            Mode::Source,
        );
        let actual = observe_source_or_register(
            &composed_source,
            Layout(0),
            u64::MAX,
            bounds,
            true,
            false,
            Mode::Specialized,
        );
        assert_eq!(actual, reference, "composed-call charge limit {charge}");
    }
    for memory in [64, 115, 165, 166, 255, 511] {
        let bounds = Bounds { memory, ..ROOMY };
        let reference = observe_source_or_register(
            &composed_source,
            Layout(0),
            u64::MAX,
            bounds,
            false,
            false,
            Mode::Source,
        );
        let actual = observe_source_or_register(
            &composed_source,
            Layout(0),
            u64::MAX,
            bounds,
            false,
            false,
            Mode::Specialized,
        );
        assert_eq!(actual, reference, "composed-call memory limit {memory}");
    }
    for call_depth in [1, 2, 3, 4] {
        let bounds = Bounds {
            call_depth,
            ..ROOMY
        };
        let reference = observe_source_or_register(
            &composed_source,
            Layout(0),
            u64::MAX,
            bounds,
            false,
            false,
            Mode::Source,
        );
        let actual = observe_source_or_register(
            &composed_source,
            Layout(0),
            u64::MAX,
            bounds,
            false,
            false,
            Mode::Specialized,
        );
        assert_eq!(actual, reference, "composed-call depth limit {call_depth}");
    }
}
