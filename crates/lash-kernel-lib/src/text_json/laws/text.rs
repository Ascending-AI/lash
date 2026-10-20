//! Text and bytes natives (FIG-6144's laws, ported onto heap values).
use lash_kernel_doc::{Datum, FunctionRegistry, Measure, Operand, Value};

use super::lab::{Lab, b, bytes, i, list, raised, run, t, unicode};

fn charge_of(name: &str, measure: &mut dyn FnMut(&Operand, Measure) -> u64) -> u64 {
    let mut registry = FunctionRegistry::new();
    super::super::register_text_json(&mut registry).unwrap();
    let (_, function) = registry
        .iter()
        .find(|(_, f)| f.definition().name.as_str() == name)
        .unwrap();
    function
        .definition()
        .charge
        .evaluate(&mut |operand, measure_kind| measure(operand, measure_kind))
}

#[test]
fn k_val_007_astral_length_and_index_have_distinct_addressing() {
    let text = t("a😀z");
    assert_eq!(run("text.len", std::slice::from_ref(&text)), i(3));
    assert_eq!(run("text.utf16_len", std::slice::from_ref(&text)), i(4));
    assert_eq!(run("text.get", &[text.clone(), i(1)]), t("😀"));
    assert_eq!(run("text.utf16_get", &[text.clone(), i(1)]), i(0xd83d));
    assert_eq!(run("text.utf16_get", &[text.clone(), i(2)]), i(0xde00));
    assert_eq!(run("text.get", &[text.clone(), i(-1)]), t("z"));
    assert_eq!(raised("text.get", &[text, i(3)]), "index_out_of_range");
    // K-LTXT-001: negative addressing applies to each selected sequence,
    // and arbitrary-precision indexes never wrap into that sequence.
    for (name, expected) in [
        ("text.get", vec![t("a"), t("😀"), t("z")]),
        ("text.utf16_get", vec![i(97), i(0xd83d), i(0xde00), i(122)]),
    ] {
        for (index, value) in expected.iter().enumerate() {
            for index in [i(index), i(index as i64 - expected.len() as i64)] {
                assert_eq!(run(name, &[t("a😀z"), index]), *value);
            }
        }
        for index in [i128::MIN, i128::MAX] {
            assert_eq!(raised(name, &[t("a😀z"), i(index)]), "index_out_of_range");
        }
    }
}

#[test]
fn k_val_007_utf16_slice_refuses_split_pair_boundaries() {
    let mut lab = Lab::new();
    assert_eq!(lab.run("text.slice", &[t("a😀z"), i(1), i(2)]), t("😀"));
    assert_eq!(
        lab.run("text.utf16_slice", &[t("a😀z"), i(1), i(3)]),
        t("😀")
    );
    for (start, end) in [(1, 2), (2, 3), (2, 2), (2, 1)] {
        assert_eq!(
            lab.raised("text.utf16_slice", &[t("a😀z"), i(start), i(end)]),
            "text_boundary"
        );
    }
    assert_eq!(lab.run("text.slice", &[t("abc"), i(-99), i(99)]), t("abc"));
    assert_eq!(lab.run("text.slice", &[t("abc"), i(2), i(1)]), t(""));
    // K-LTXT-002 validates both normalized boundaries, even when they
    // address different ends or describe an empty/reversed interval.
    for text in ["", "abc", "😀a😀", "a😀z"] {
        let units: Vec<_> = text.encode_utf16().collect();
        let normalize = |index: i32| {
            (if index < 0 {
                index + units.len() as i32
            } else {
                index
            })
            .clamp(0, units.len() as i32) as usize
        };
        for start in -7..=7 {
            for end in -7..=7 {
                let (a, z) = (normalize(start), normalize(end));
                let args = [t(text), i(start), i(end)];
                if String::from_utf16(&units[..a]).is_err()
                    || String::from_utf16(&units[..z]).is_err()
                {
                    assert_eq!(
                        lab.raised("text.utf16_slice", &args),
                        "text_boundary",
                        "{text:?}[{start}:{end}]"
                    );
                } else {
                    let expected = if a > z {
                        String::new()
                    } else {
                        String::from_utf16(&units[a..z]).unwrap()
                    };
                    assert_eq!(
                        lab.run("text.utf16_slice", &args),
                        t(&expected),
                        "{text:?}[{start}:{end}]"
                    );
                }
            }
        }
    }
    let enormous = num_bigint::BigInt::from(i128::MAX) + 1u8;
    for name in ["text.slice", "text.utf16_slice"] {
        assert_eq!(
            lab.run(name, &[t("a😀z"), i(-&enormous), i(enormous.clone())]),
            t("a😀z")
        );
    }
}

#[test]
fn k_ltxt_003_search_reports_the_selected_address_space() {
    assert_eq!(run("text.find", &[t("😀z😀z"), t("z"), i(2)]), i(3));
    assert_eq!(run("text.utf16_find", &[t("😀z😀z"), t("z"), i(3)]), i(5));
    assert_eq!(run("text.utf16_find", &[t("😀z"), t("😀"), i(1)]), i(-1));
    assert_eq!(run("text.find", &[t("😀z"), t(""), i(99)]), i(2));
    assert_eq!(run("text.utf16_find", &[t("😀z"), t(""), i(99)]), i(3));
    // K-LTXT-003 allows a UTF-16 start inside a pair without manufacturing
    // surrogate text, in either relative addressing direction.
    for start in [1, -2] {
        assert_eq!(run("text.utf16_find", &[t("😀z"), t(""), i(start)]), i(1));
        assert_eq!(run("text.utf16_find", &[t("😀z"), t("z"), i(start)]), i(2));
        assert_eq!(
            run("text.utf16_find", &[t("😀z"), t("😀"), i(start)]),
            i(-1)
        );
    }
}

#[test]
fn k_val_030_code_point_order_differs_from_unit_order() {
    assert_eq!(run("text.compare", &[t("😀"), t("\u{e000}")]), i(1));
    assert_eq!(run("text.utf16_compare", &[t("😀"), t("\u{e000}")]), i(-1));
    assert_eq!(run("text.compare", &[t("a"), t("aa")]), i(-1));
}

#[test]
fn k_ltxt_005_literal_split_join_replace_and_concat() {
    assert_eq!(run("text.concat", &[t("a"), t("😀")]), t("a😀"));
    let pieces = run("text.split", &[t("a..b.."), t("..")]);
    assert_eq!(pieces, list(vec![t("a"), t("b"), t("")]));
    assert_eq!(run("text.join", &[pieces, t("|")]), t("a|b|"));
    assert_eq!(
        run("text.split", &[t("😀a"), t("")]),
        list(vec![t("😀"), t("a")])
    );
    assert_eq!(run("text.split", &[t(""), t("")]), list(vec![]));
    // A limit keeps the first pieces; an absent one keeps them all.
    assert_eq!(
        run("text.split", &[t("a..b.."), t(".."), i(2)]),
        list(vec![t("a"), t("b")])
    );
    assert_eq!(
        run("text.split", &[t("😀a"), t(""), i(1)]),
        list(vec![t("😀")])
    );
    assert_eq!(
        run("text.split", &[t("a.b"), t("."), Datum::Absent]),
        list(vec![t("a"), t("b")])
    );
    assert_eq!(
        raised("text.split", &[t("a.b"), t("."), i(-1)]),
        "number_range"
    );
    assert_eq!(run("text.replace", &[t("aaa"), t("aa"), t("x")]), t("xa"));
    assert_eq!(run("text.replace", &[t("😀a"), t(""), t("-")]), t("-😀-a-"));
    assert_eq!(run("text.join", &[list(vec![]), t("|")]), t(""));
    assert_eq!(
        raised("text.join", &[list(vec![t("a"), i(1)]), t("|")]),
        "type_error"
    );
}

#[test]
fn k_ltxt_006_case_and_whitespace_are_versioned_and_locale_free() {
    assert_eq!(run(&unicode("lower"), &[t("İΟΣ")]), t("i\u{307}ος"));
    assert_eq!(run(&unicode("upper"), &[t("straße")]), t("STRASSE"));
    assert_eq!(run(&unicode("trim"), &[t("\u{2003} a \u{85}")]), t("a"));
    assert_eq!(run(&unicode("trim_start"), &[t("\u{2003}a ")]), t("a "));
    assert_eq!(run(&unicode("trim_end"), &[t(" a\u{85}")]), t(" a"));
    assert_eq!(
        run(&unicode("trim"), &[t("\u{feff}a\u{feff}")]),
        t("\u{feff}a\u{feff}")
    );
}

#[test]
fn k_ltxt_007_repeat_and_padding_count_scalars() {
    assert_eq!(run("text.repeat", &[t("😀a"), i(2)]), t("😀a😀a"));
    assert_eq!(run("text.repeat", &[t("a"), i(0)]), t(""));
    assert_eq!(raised("text.repeat", &[t("a"), i(-1)]), "number_range");
    assert_eq!(run("text.pad_start", &[t("😀"), i(4), t("ab")]), t("aba😀"));
    assert_eq!(run("text.pad_end", &[t("😀"), i(3), t("xy")]), t("😀xy"));
    assert_eq!(run("text.pad_start", &[t("abc"), i(2), t("x")]), t("abc"));
    assert_eq!(run("text.pad_end", &[t("a"), i(9), t("")]), t("a"));
}

#[test]
fn k_ltxt_008_prefix_suffix_and_empty_patterns() {
    assert_eq!(run("text.starts_with", &[t("😀a"), t("😀")]), b(true));
    assert_eq!(run("text.ends_with", &[t("😀a"), t("a")]), b(true));
    assert_eq!(run("text.ends_with", &[t("a"), t("")]), b(true));
    assert_eq!(run("text.starts_with", &[t("a"), t("b")]), b(false));
}

#[test]
fn k_val_006_scalar_and_utf16_conversion_refuse_lone_surrogates() {
    for (to, from, expected) in [
        (
            "text.to_code_points",
            "text.from_code_points",
            vec![i(0x1f600), i(97)],
        ),
        (
            "text.to_utf16_units",
            "text.from_utf16_units",
            vec![i(0xd83d), i(0xde00), i(97)],
        ),
    ] {
        let converted = run(to, &[t("😀a")]);
        assert_eq!(converted, list(expected));
        assert_eq!(run(from, &[converted]), t("😀a"));
    }
    let bad = list(vec![i(0xd800)]);
    assert_eq!(
        raised("text.from_code_points", std::slice::from_ref(&bad)),
        "invalid_scalar"
    );
    assert_eq!(raised("text.from_utf16_units", &[bad]), "invalid_utf16");
    assert_eq!(
        raised("text.from_code_points", &[list(vec![i(0x110000)])]),
        "invalid_scalar"
    );
}

#[test]
fn k_val_008_octets_slice_concat_and_compare() {
    let octets = run("bytes.from_octets", &[list(vec![i(0), i(128), i(255)])]);
    assert_eq!(octets, bytes(&[0, 128, 255]));
    assert_eq!(
        run("bytes.slice", &[octets.clone(), i(-2), i(99)]),
        bytes(&[128, 255])
    );
    assert_eq!(
        run("bytes.concat", &[octets.clone(), bytes(&[1])]),
        bytes(&[0, 128, 255, 1])
    );
    assert_eq!(
        run("bytes.compare", &[octets, bytes(&[0, 128, 255, 1])]),
        i(-1)
    );
    assert_eq!(
        raised("bytes.from_octets", &[list(vec![i(256)])]),
        "number_range"
    );
}

#[test]
fn k_lbytes_002_utf8_is_strict_without_replacement() {
    let encoded = run("bytes.utf8_encode", &[t("😀\0")]);
    assert_eq!(encoded, bytes(&[0xf0, 0x9f, 0x98, 0x80, 0]));
    assert_eq!(run("bytes.utf8_decode", &[encoded]), t("😀\0"));
    for octets in [
        vec![0xc0, 0x80],
        vec![0xed, 0xa0, 0x80],
        vec![0xf0, 0x9f],
        vec![0xff],
    ] {
        assert_eq!(
            raised("bytes.utf8_decode", &[bytes(&octets)]),
            "invalid_utf8"
        );
    }
}

#[test]
fn k_val_001_native_domains_never_coerce() {
    assert_eq!(raised("text.len", &[i(1)]), "type_error");
    assert_eq!(
        raised("text.get", &[t("a"), super::lab::f(0.0)]),
        "type_error"
    );
    assert_eq!(
        raised("text.join", &[list(vec![i(1)]), t(",")]),
        "type_error"
    );
    assert_eq!(
        raised("bytes.slice", &[t("bytes"), i(0), i(1)]),
        "type_error"
    );
    assert_eq!(raised("text.len", &[]), "arity");
    assert_eq!(raised("text.len", &[t("a"), t("b")]), "arity");
}

/// K-LTXT-011: a caller supplies the scalar set and the ends; the product
/// charge covers every membership probe and the copy, reserved before allocation.
#[test]
fn k_ltxt_011_trim_set_bounds_scan_copy_and_reserves_output() {
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
                let args = [t(source), t(characters), b(leading), b(trailing)];
                let mut lab = Lab::new();
                assert_eq!(lab.run("text.trim_set", &args), t(expected));
                assert_eq!(lab.reserved, 2 * expected.len() as u64);
                let charged = charge_of("text.trim_set", &mut |operand, _| match operand {
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
                    lab.room(Some(2 * expected.len() as u64 - 1));
                    let args = lab.values(&args);
                    assert!(crate::testsupport::exhausted(
                        &lab.call("text.trim_set", &args)
                    ));
                    assert_eq!(lab.reserved, 0);
                }
            }
        }
    }
    for args in [
        vec![t("x")],
        vec![Datum::Null, t(""), b(true), b(true)],
        vec![t("x"), t(""), Datum::Null, b(true)],
    ] {
        let expected = if args.len() == 1 {
            "arity"
        } else {
            "type_error"
        };
        assert_eq!(raised("text.trim_set", &args), expected);
    }
}

/// K-LTXT-012: admission stops at the first non-text slot without coercion,
/// and its shallow formula bounds both scans and the reserved output copy.
#[test]
fn k_ltxt_012_join_admission_bounds_scan_copy_and_first_mismatch() {
    for pieces in [
        vec![],
        vec![""],
        vec!["", "😀", "é", ""],
        vec!["x"; 32],
        vec!["\0é😀é😀x", "é😀é😀é😀", "", "row-499"],
    ] {
        for separator in ["", "|", "😀"] {
            let mut lab = Lab::new();
            let input = list(pieces.iter().map(|piece| t(piece)).collect());
            let expected = pieces.join(separator);
            assert_eq!(
                lab.run("text.join_or_index", &[input.clone(), t(separator)]),
                t(&expected)
            );
            assert_eq!(lab.reserved, 2 * expected.len() as u64);
            let charge = charge_of("text.join_or_index", &mut |operand, measure| {
                assert_eq!(
                    measure,
                    Measure::Size,
                    "admission never walks nested values"
                );
                match operand {
                    Operand::Param(name) if name.as_str() == "items" => 1 + pieces.len() as u64,
                    Operand::Param(_) => 1 + separator.len() as u64,
                    Operand::Result => 1 + expected.len() as u64,
                }
            });
            assert_eq!(
                charge,
                1 + 2 * (1 + pieces.len() as u64)
                    + 1
                    + separator.len() as u64
                    + 2 * (1 + expected.len() as u64)
            );
            assert!(charge >= 2 * pieces.len() as u64 + 2 * expected.len() as u64);
            if !expected.is_empty() {
                let args = lab.values(&[input, t(separator)]);
                lab.room(Some(2 * expected.len() as u64 - 1));
                assert!(crate::testsupport::exhausted(
                    &lab.call("text.join_or_index", &args)
                ));
                assert_eq!(lab.reserved, 0);
            }
        }
    }
    for index in [0, 1, 31] {
        for mismatch in [Datum::Absent, Datum::Null, Datum::Tuple(vec![]), i(9)] {
            let mut lab = Lab::new();
            let mut pieces = vec![t("😀"); 32];
            pieces[index] = mismatch;
            pieces[31] = Datum::Null;
            let args = lab.values(&[list(pieces), t("|")]);
            lab.room(Some(0));
            let result = lab.call("text.join_or_index", &args).unwrap();
            assert_eq!(lab.datum(&result), i(index));
            assert_eq!(lab.reserved, 0);
        }
    }
    // A list that holds itself is a non-text slot, never walked.
    let mut lab = Lab::new();
    let cycle = lab.cycle();
    let (a, z, separator) = (lab.value(t("a")), lab.value(t("z")), lab.value(t("|")));
    let input = lab.list_of(vec![a, cycle, z]);
    let result = lab.call("text.join_or_index", &[input, separator]).unwrap();
    assert_eq!(lab.datum(&result), i(1));
    // Native member admission precedes separator validation and allocation.
    let input = lab.value(list(vec![t("a"), Datum::Null]));
    let result = lab
        .call("text.join_or_index", &[input, Value::Null])
        .unwrap();
    assert_eq!(lab.datum(&result), i(1));
    for args in [vec![Datum::Null, t("|")], vec![list(vec![]), Datum::Null]] {
        assert_eq!(lab.raised("text.join_or_index", &args), "type_error");
    }
}
