//! K-EVAL-008 and K-ID-004 bind constructor contents to source roles.
use super::*;
use crate::compile::room;
use lash_kernel_doc as doc;
use lash_kernel_value::sites::{Site, Unit};
use std::sync::Arc;

#[test]
fn source_constructor_certificates_authenticate_values() {
    let mut failures = Vec::new();
    for expression in [
        "cell",
        "[1, 2]",
        "(1, 2)",
        "map{1: 2}",
        "set{1, 2}",
        "{left: 1, rght: 2}",
        "\"a deferred literal larger than the inline text capacity\"",
    ] {
        let run = Embedder::new(&format!(
            "main {{ let made = {} return made }}",
            if expression == "cell" {
                "1"
            } else {
                expression
            }
        ));
        let (program, _) = run.program(false);
        let source = Arc::new(room::Source::compile(
            doc::constant_pool::PoolOwner::Document(program.document.identity()),
            Site::new(Unit::Main, vec![]),
            program.document.main.as_ref().unwrap(),
            &[],
            if expression == "cell" {
                [(
                    (
                        Site::new(Unit::Main, vec![0]),
                        lash_kernel_value::Name::new("made"),
                    ),
                    true,
                )]
                .into()
            } else {
                Default::default()
            },
            &std::collections::BTreeMap::new(),
            &[lash_kernel_value::Name::new("made")].into(),
        ));
        failures.extend(run.machine.check_source_constructor_certificates(
            &source,
            program.link,
            expression,
        ));
    }
    assert!(
        failures.is_empty(),
        "source constructor authentication failures: {failures:?}"
    );
}

/// K-CLO-001 and K-EVAL-008 preserve real capture aliases across layouts and reopen.
#[test]
fn constructors_and_capture_aliases_agree() {
    use crate::laws::embedder::ROOMY;
    const BODY: &str = "let run = fn(value) { let left = [value] let right = [value] let getter = fn() { return (right, left) } let alias = getter let empty_a = () let empty_b = () let values = [left, right] let pair = (left, right) let table = map{1: left, 1.0: right} let members = set{1, 1.0, 2} let record = {rght: right, left: left} do \"a deferred literal larger than the inline text capacity\" do 1000000000000000000000000 do b\"0011ff\" do perform 0(values) as Any set right = left let got = apply alias() return (got, values, pair, table, members, record, empty_a, empty_b) } return apply run(7)";
    const NATIVE: &str = "let f = fn(value) { return resident.len([value, value, value, value, value, value, value, value, value, value, value, value, value, value, value, value, value, value, value, value, value, value, value, value, value, value, value, value, value, value, value, value]) } return apply f(7)";
    const RETURN: &str = "let f = fn(value) { return [value, value, value, value, value, value, value, value, value, value, value, value, value, value, value, value, value, value, value, value, value, value, value, value, value, value, value, value, value, value, value, value] } return apply f(7)";
    const SESSION: &str = "let saved = [7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7] set saved = [saved, saved] return saved";

    for body in [SOURCE_CELL, BODY, NATIVE, RETURN, SESSION] {
        let text = format!("main {{ {body} }}");
        for layout in [Layout(0), Layout(2), Layout(0x5eed)] {
            for taped in [false, true] {
                let reference = observe_source_or_region(
                    &text,
                    layout,
                    u64::MAX,
                    ROOMY,
                    taped,
                    false,
                    Mode::Source,
                );
                for mode in [Mode::Generic, Mode::Specialized] {
                    assert_eq!(
                        observe_source_or_region(
                            &text,
                            layout,
                            u64::MAX,
                            ROOMY,
                            taped,
                            false,
                            mode
                        ),
                        reference
                    );
                }
                for slice in [1, 3, 7] {
                    let reference = observe_source_or_region(
                        &text,
                        layout,
                        slice,
                        ROOMY,
                        taped,
                        true,
                        Mode::Source,
                    );

                    assert_eq!(
                        observe_source_or_region(
                            &text,
                            layout,
                            slice,
                            ROOMY,
                            taped,
                            true,
                            Mode::Specialized
                        ),
                        reference
                    );
                }
            }
        }
        let reference =
            observe_source_or_region(&text, Layout(0), u64::MAX, ROOMY, true, false, Mode::Source);
        let charged = reference.stops.last().unwrap().1.charged.unwrap();
        for limit in 0..=charged {
            let bounds = Bounds {
                charge_limit: limit,
                ..ROOMY
            };
            let source = observe_source_or_region(
                &text,
                Layout(0),
                u64::MAX,
                bounds,
                true,
                false,
                Mode::Source,
            );
            for mode in [Mode::Generic, Mode::Specialized] {
                assert_eq!(
                    observe_source_or_region(&text, Layout(0), u64::MAX, bounds, true, false, mode),
                    source,
                    "charge {limit}"
                );
            }
        }
        let mut refused = false;
        for memory in [256, 512, 1024, 2048, 4096, 8192] {
            let bounds = Bounds { memory, ..ROOMY };
            let source = observe_source_or_region(
                &text,
                Layout(0),
                u64::MAX,
                bounds,
                true,
                false,
                Mode::Source,
            );
            refused |= source.end.contains("Memory");
            for mode in [Mode::Generic, Mode::Specialized] {
                assert_eq!(
                    observe_source_or_region(&text, Layout(0), u64::MAX, bounds, true, false, mode),
                    source,
                    "memory {memory}"
                );
            }
        }
        assert!(
            refused,
            "a constructor must reach a real memory refusal: {body}"
        );
    }
}

/// K-CLO-001: equal-sized cells and equal capture counts do not authorize aliases.
#[test]
fn closure_certificates_authenticate_source_cells() {
    let run = Embedder::new(
        "main { let make = fn(left, right) { let grab = fn() { return (right, left) } return grab } return apply make(7, 7) }",
    );
    let (program, _) = run.program(false);
    let admitted = program
        .library
        .admit(&program.document, Default::default(), &program.selection)
        .unwrap_or_else(|_| panic!("capture source admitted"));
    let exe = admitted
        .executable(
            &program.document,
            &program.library,
            &program.documents,
            &program.functions,
            &program.instances,
        )
        .unwrap_or_else(|_| panic!("capture source compiled"));
    let plan = (0..exe.code_count())
        .map(|n| crate::compile::CodeId(n as u32))
        .find(|code| {
            exe.code(*code).params.len() == 2 && exe.block(exe.code(*code).body).stmts.len() == 2
        })
        .unwrap();
    let source = &exe.code(plan).source_room;
    let (stream, stmt, _) = exe.statement(exe.block(exe.code(plan).body).stmts[0]);
    let (pc, code) = (stmt.ops.start..stmt.ops.end())
        .find_map(|pc| match stream.ops[pc as usize] {
            lash_kernel_exec::Op::Closure { code, .. } => Some((pc, code)),
            _ => None,
        })
        .unwrap();
    let target = exe.code(crate::compile::CodeId(code.0));
    let at = stream.source_sites[pc as usize].as_ref().unwrap();
    let lash_kernel_value::sites::Unit::Library(function) = target.site.unit else {
        panic!("closure body");
    };
    let identity = Arc::new(doc::arena::ClosureIdentity {
        capture_names: target.capture_names.clone(),
        callable: lash_kernel_value::names::CallableId {
            link: program.link,
            function,
        },
        site: doc::linked_site::LinkedSite {
            link: program.link,
            site: (**at).clone(),
        },
    });
    let names = target
        .captures
        .iter()
        .map(|capture| target.slots[capture.inner as usize].name.clone())
        .collect();
    run.machine.check_source_closure_certificates(
        source,
        identity,
        names,
        doc::arena::LocalCode {
            executable: exe.local(),
            code: code.0,
        },
    );
}
