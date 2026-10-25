//! Physical register windows preserve source slots and captures.

use super::*;

/// K-EVAL-008 / K-MACH-008 / K-CHG-001: wide windows keep source
/// slots and captured cells at their semantic coordinates, including the
/// operands of a waiting caller, every finite-slice exit and cold reopen.
#[test]
fn wide_windows_agree() {
    let definition = |name: &str, body: &str| {
        let function =
            format!("function {name} -> Any\nkernel 1\nmetering statements\nbody {{ {body} }}");
        format!("definition {}\n", serde_json::to_string(&function).unwrap())
    };
    let run = |source: &str, layout, cold, bounds| {
        let mut embedder = Embedder::with(
            source,
            Setup {
                layout,
                bounds,
                probes: Some(scalar_probes),
                slice: if cold { 31 } else { u64::MAX },
                ..Setup::default()
            },
        );
        let tape = crate::ChargeTape::new();
        embedder.machine.record_charges(tape.clone());
        let mut checkpoints = Vec::new();
        let end = loop {
            let step = embedder.run();
            if let Step::Ended(mut end) = step {
                if let End::Finished(finished) = &mut end {
                    finished.carry.take();
                }
                break end;
            }
            if matches!(step, Step::Parked(_)) {
                checkpoints.push(embedder.machine.semantic_checkpoint());
            }
            if cold {
                embedder.reopen(layout, true);
                embedder.machine.record_charges(tape.clone());
            }
            if matches!(step, Step::Parked(_)) {
                embedder.deliver_next();
            }
        };
        (
            (
                end,
                embedder.world.printed,
                embedder.requests,
                embedder.machine.meters().charged.unwrap(),
                lash_kernel_exec::Weight::expand(tape.steps()),
                checkpoints,
            ),
            crate::kernel_internals::region_work(&tape).calls,
        )
    };
    let bounds = Bounds {
        memory: 256 << 10,
        ..super::super::embedder::ROOMY
    };
    // K-EVAL-008: small recursive frames retain wide evaluated prefixes.
    // Crossing the transient-window reservation must materialize all those
    // operands in source order without changing charge or results.
    let params = (0..260)
        .map(|i| format!("p{i}: Any"))
        .collect::<Vec<_>>()
        .join(", ");
    let args = std::iter::repeat_n("\"01234567890123\"", 259)
        .collect::<Vec<_>>()
        .join(", ");
    let source = format!(
        "{}main {{ let recur = fn(self, n) {{ if flat.lt(n, 1) {{ return 0 }} return call operands({args}, apply self(self, flat.add(n, -1))) }} let answer = apply recur(recur, 100) print answer return answer }}",
        definition(&format!("operands({params})"), "return p259"),
    );
    let reference = run(&source, Layout(0).without_regions(), false, bounds).0;
    assert_eq!(result(reference.0.clone()), int(0));
    assert_eq!(run(&source, Layout(0), false, bounds).0, reference);
    let mut comparisons = 0;
    for width in [17, 260] {
        let params = (0..width)
            .map(|i| format!("p{i}: Any"))
            .collect::<Vec<_>>()
            .join(", ");
        let args = (0..width)
            .map(|i| i.to_string())
            .collect::<Vec<_>>()
            .join(", ");
        let sum = (1..width)
            .map(|i| format!("set p0 = flat.add(p0, p{i})\n"))
            .collect::<String>();
        let source = format!(
            "{}{}main {{ return call caller() }}",
            definition(&format!("wide({params})"), &format!("{sum} return p0")),
            definition(
                "caller()",
                &format!("return flat.add(1000, call wide({args}))")
            ),
        );
        let (natural, entered) = run(&source, Layout(0), false, super::super::embedder::ROOMY);
        assert_eq!(
            result(natural.0.clone()),
            int(1000 + width * (width - 1) / 2)
        );
        assert!(
            entered >= 2,
            "the {width}-argument caller and callee must enter registers"
        );
        for layout in [Layout(0), Layout(2), Layout(0x5eed)] {
            let reference = run(
                &source,
                layout.without_regions(),
                false,
                super::super::embedder::ROOMY,
            )
            .0;
            for cold in [false, true] {
                assert_eq!(
                    run(&source, layout, cold, super::super::embedder::ROOMY).0,
                    reference
                );
                comparisons += 1;
            }
        }
    }
    for width in [17, 260] {
        let locals = (0..width)
            .map(|i| format!("let v{i} = {i}\n"))
            .collect::<String>();
        // No other document callee can satisfy the entry witness: this
        // function itself must run through its wide register window.
        let simple = format!(
            "{}main {{ let answer = call wide(1) print answer return answer }}",
            definition(
                "wide(x: Any)",
                &format!("{locals} return flat.add(x, v{})", width - 1),
            )
        );
        let (natural, entered) = run(&simple, Layout(0), false, super::super::embedder::ROOMY);
        assert_eq!(result(natural.0.clone()), int(width));
        assert!(entered > 0, "a {width}-slot function must enter registers");
        let parked = format!(
            "{}{}main {{ return call wide(true) }}",
            definition(
                "relay(x: Any, pause: Any)",
                "if pause { do perform 0(\"wide\") as Any } return flat.add(x, 1)",
            ),
            definition(
                "wide(pause: Any)",
                &format!(
                    "{locals} return flat.add(v1, call relay(v{}, pause))",
                    width - 1
                ),
            ),
        );
        let recursive = format!(
            "main {{ let wide = fn(self, n) {{ {locals} if flat.lt(n, 1) {{ return v1 }} return flat.add(v1, apply self(self, flat.add(n, -1))) }} let answer = apply wide(wide, 12) print answer return answer }}"
        );
        let closure = format!(
            "{}main {{ let f = call make() return apply f() }}",
            definition(
                "make()",
                &format!(
                    "{locals} let f = fn() {{ {} return flat.add(a0, a{}) }} return f",
                    (0..width)
                        .map(|i| format!("let a{i} = v{i}\n"))
                        .collect::<String>(),
                    width - 1,
                ),
            ),
        );
        for source in [&simple, &parked, &recursive, &closure] {
            let reference = run(
                source,
                Layout(0).without_regions(),
                false,
                super::super::embedder::ROOMY,
            )
            .0;
            for layout in [Layout(0), Layout(2), Layout(0x5eed)] {
                assert_eq!(
                    run(source, layout, false, super::super::embedder::ROOMY).0,
                    reference
                );
                comparisons += 1;
                // Cold wide callers retain their live operands at a park.
                // Recursive budget exits are exercised whole above.
                if source != &recursive {
                    assert_eq!(
                        run(source, layout, true, super::super::embedder::ROOMY).0,
                        reference
                    );
                    comparisons += 1;
                }
            }
        }
        // Refusals just before/after frame entry and at a later operation
        // must preserve the actual ordered event prefix and no later effect.
        for charge_limit in [0, 1, 2, natural.3 - 1, natural.3] {
            let bounds = Bounds {
                charge_limit,
                ..super::super::embedder::ROOMY
            };
            assert_eq!(
                run(&simple, Layout(0), false, bounds).0,
                run(&simple, Layout(0).without_regions(), false, bounds).0,
            );
            comparisons += 1;
        }
    }
    // K-BND-003: the same entry reservation must bound all live wide
    // frames. Each caller needs every local after the recursive call, so
    // their holdings cannot disappear through a statement kill set.
    let locals = (0..260)
        .map(|i| format!("let v{i} = {i}\n"))
        .collect::<String>();
    let sums = (0..260)
        .map(|i| format!("set total = flat.add(total, v{i})\n"))
        .collect::<String>();
    let source = format!(
        "main {{ let wide = fn(self, n) {{ {locals} if flat.lt(n, 1) {{ return 0 }} let total = apply self(self, flat.add(n, -1)) {sums} return total }} let answer = apply wide(wide, 100) print answer return answer }}"
    );
    let bounds = Bounds {
        memory: 256 << 10,
        ..super::super::embedder::ROOMY
    };
    for layout in [Layout(0).without_regions(), Layout(0)] {
        let (seen, _) = run(&source, layout, false, bounds);
        assert!(matches!(
            seen.0,
            End::Error(crate::RunError::Bound(crate::BoundExceeded {
                bound: crate::Bound::Memory,
                ..
            }))
        ));
        assert!(
            seen.1.is_empty(),
            "a memory refusal precedes the later print"
        );
    }
    println!("wide windows: {comparisons} paired runs, two memory refusals");
}
