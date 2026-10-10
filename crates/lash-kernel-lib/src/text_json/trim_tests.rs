use super::*;

/// K-LTXT-011: a caller supplies the scalar set and the ends; the product
/// charge covers every membership probe and the copy, reserved before allocation.
#[test]
fn k_ltxt_011_trim_set_bounds_scan_copy_and_reserves_output() {
    use lash_kernel_doc::Operand;
    let mut registry = FunctionRegistry::new();
    register_text_json(&mut registry).unwrap();
    let (_, registered) = registry
        .iter()
        .find(|(_, f)| f.definition.name.as_str() == "text.trim_set")
        .unwrap();
    for source in [
        "",
        "aaaa",
        "aa😀aaa",
        "😀a😀",
        "x\u{85}x",
        "\u{feff}x\u{feff}",
    ] {
        for characters in ["", "a", "😀", "a😀", "\u{85}", "\u{feff}"] {
            for (leading, trailing) in [(false, false), (true, false), (false, true), (true, true)]
            {
                let mut expected = source;
                if leading {
                    expected = expected.trim_start_matches(|c| characters.contains(c));
                }
                if trailing {
                    expected = expected.trim_end_matches(|c| characters.contains(c));
                }
                let args = [
                    s(source),
                    s(characters),
                    Value::Bool(leading),
                    Value::Bool(trailing),
                ];
                let mut heap = Heap::default();
                let result = call(&mut heap, "text.trim_set", &args).unwrap();
                assert_eq!(result, s(expected));
                assert_eq!(heap.1.reserved, 2 * expected.len() as u64);
                let charged =
                    registered
                        .definition
                        .charge
                        .evaluate(&mut |operand, _| match operand {
                            Operand::Result => 1 + expected.len() as u64,
                            Operand::Param(name) => match name.as_str() {
                                "text" => 1 + source.len() as u64,
                                "characters" => 1 + characters.len() as u64,
                                _ => 1,
                            },
                        });
                assert_eq!(
                    charged,
                    8 + (2 + source.len() as u64) * (2 + characters.len() as u64)
                        + 1
                        + expected.len() as u64
                );
                // At most input-scalars + one failed boundary probe. A probe
                // scans the set's bytes; decoding and copying are also bounded.
                assert!(
                    charged
                        >= 8 + source.len() as u64 * (1 + characters.len() as u64)
                            + expected.len() as u64
                );
                if !expected.is_empty() {
                    heap.1 = Room::of(2 * expected.len() as u64 - 1);
                    assert_eq!(
                        call(&mut heap, "text.trim_set", &args),
                        Err(NativeError::Memory)
                    );
                    assert_eq!(heap.1.reserved, 0);
                }
            }
        }
    }
    for args in [
        vec![s("x")],
        vec![Value::Null, s(""), Value::Bool(true), Value::Bool(true)],
        vec![s("x"), s(""), Value::Null, Value::Bool(true)],
    ] {
        let expected = if args.len() == 1 {
            "arity"
        } else {
            "type_error"
        };
        assert_eq!(
            error_kind(call(&mut Heap::default(), "text.trim_set", &args)),
            expected
        );
    }
}
