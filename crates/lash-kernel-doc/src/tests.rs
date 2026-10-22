//! Laws independent of replaced document/manifest/entry APIs.
use crate::arena::ShortText;
use crate::{
    Float, Formula, FunctionDefinition, FunctionRegistry, InvalidReason, RegistryError,
    ReleasedFunctions, parse_definition, print_definition,
};

#[test]
fn short_text_concat_and_ascii_match_new() {
    for len in 0..=ShortText::MAX + 1 {
        for byte in 0..=127 {
            let bytes = vec![byte; len];
            let text = std::str::from_utf8(&bytes).unwrap();
            let expected = ShortText::new(text);
            let actual = ShortText::ascii(&bytes);
            assert_eq!(actual, expected);
            assert_eq!(actual.map(|s| s.word()), expected.map(|s| s.word()));
        }
    }
    for byte in 128..=255 {
        assert!(ShortText::ascii(&[byte]).is_none());
    }
    let texts = [
        "",
        "\0",
        "abc",
        "é",
        "界",
        "🦀",
        "0123456789🦀",
        "\0é界🦀abcd",
    ];
    for head in texts {
        for tail in texts {
            let expected = ShortText::new(&format!("{head}{tail}"));
            let actual = ShortText::new(head)
                .unwrap()
                .concat(&ShortText::new(tail).unwrap());
            assert_eq!(actual, expected);
            assert_eq!(actual.map(|s| s.word()), expected.map(|s| s.word()));
        }
    }
}

#[test]
fn short_text_as_str_round_trips_every_constructor() {
    for text in [
        "",
        "\0abc\0",
        "01234567890123",
        "é界",
        "0123456789🦀",
        "🦀abcdef🦀",
    ] {
        assert_eq!(ShortText::new(text).unwrap().as_str(), text);
        if text.is_ascii() {
            assert_eq!(ShortText::ascii(text.as_bytes()).unwrap().as_str(), text);
        }
        for split in 0..=text.len() {
            if text.is_char_boundary(split) {
                let head = ShortText::new(&text[..split]).unwrap();
                let tail = ShortText::new(&text[split..]).unwrap();
                assert_eq!(head.concat(&tail).unwrap().as_str(), text);
            }
        }
    }
}

fn definition(name: &str, native: bool) -> FunctionDefinition {
    let text = if native {
        format!("function {name}(x: Any) -> Any\nkernel 1\ncharge 1\nnative\n")
    } else {
        format!("function {name}(x: Any) -> Any\nkernel 1\ncharge 1\nbody {{ return x }}\n")
    };
    parse_definition(&text).unwrap()
}

#[test]
fn float_text_is_shortest_round_trip_in_the_pinned_layout() {
    let cases = [
        (0.0, "0.0"),
        (-0.0, "-0.0"),
        (1.0, "1.0"),
        (-1.5, "-1.5"),
        (0.1, "0.1"),
        (0.1 + 0.2, "0.30000000000000004"),
        (1e-4, "0.0001"),
        (9.999e-5, "9.999e-5"),
        (1e15, "1000000000000000.0"),
        (9007199254740993.0, "9007199254740992.0"),
        (1e16, "1e16"),
        (1.5e300, "1.5e300"),
        (5e-324, "5e-324"),
        (f64::MAX, "1.7976931348623157e308"),
        (f64::INFINITY, "inf"),
        (f64::NEG_INFINITY, "-inf"),
        (f64::NAN, "nan"),
    ];
    for (value, text) in cases {
        let float = Float::new(value);
        assert_eq!(float.to_string(), text);
        assert_eq!(Float::parse(text).unwrap(), float);
    }
    // Only the canonical spelling is stored.
    for text in [
        "1", "1.", "01.0", "1.50", "1e5", "+1.0", "NaN", "1E16", "1.0e16",
    ] {
        assert!(Float::parse(text).is_err(), "{text}");
    }
}

#[test]
fn a_formula_saturates() {
    let formula = Formula::Product(vec![Formula::Constant(u64::MAX), Formula::Constant(2)]);
    assert_eq!(formula.evaluate(&mut |_, _| 0), u64::MAX);
}

#[test]
fn validated_functions_join_only_the_registry_they_were_validated_against() {
    let holding = |name: &str| {
        let mut registry = FunctionRegistry::new();
        registry.register(definition(name, false), None).unwrap();
        registry
    };
    let mut built = holding("helper.base");
    let validated = built
        .validate_functions(vec![definition("helper.joined", false)])
        .unwrap();
    let image: &'static [u8] = Vec::leak(validated.to_image(&|_| None));
    let validated = ReleasedFunctions::read(image).unwrap();
    let mut same = holding("helper.base");
    same.register_released(validated).unwrap();
    assert!(
        same.iter()
            .map(|(id, _)| id)
            .eq(built.iter().map(|(id, _)| id))
    );
    let mut wider = holding("helper.base");
    wider
        .register(definition("helper.other", false), None)
        .unwrap();
    wider.register_released(validated).unwrap();
    assert_eq!(wider.len(), built.len() + 1);
    for mut other in [holding("helper.other"), FunctionRegistry::new()] {
        let held = other.len();
        assert!(matches!(
            other.register_released(validated),
            Err(RegistryError::OtherBasis { .. })
        ));
        assert_eq!(other.len(), held, "nothing joins another registry");
    }
}

#[test]
fn a_native_version_is_part_of_the_identity_and_only_a_native_states_one() {
    let first = definition("num.twice", true);
    assert_eq!(first.native_version, crate::FIRST_NATIVE_VERSION);
    assert!(
        !first.to_json().unwrap().contains("native_version"),
        "the first version is not written"
    );
    let text = "function num.twice(x: Any) -> Any\nkernel 1\ncharge 1\nnative 2\n";
    let second = parse_definition(text).unwrap();
    assert_eq!(second.native_version, 2);
    assert_eq!(print_definition(&second), text);
    assert_ne!(second.identity(), first.identity());
    let empty = FunctionRegistry::new();
    crate::validate_definition(&second, &empty).expect("a native states its version");
    let body = FunctionDefinition {
        native_version: 2,
        ..definition("helper.same", false)
    };
    assert!(matches!(
        crate::validate_definition(&body, &empty).map_err(|invalid| *invalid.reason),
        Err(InvalidReason::NativeVersionWithoutNative { version: 2 })
    ));
    let zero = FunctionDefinition {
        native_version: 0,
        ..first
    };
    assert!(matches!(
        crate::validate_definition(&zero, &empty).map_err(|invalid| *invalid.reason),
        Err(InvalidReason::NativeVersionZero)
    ));
}

#[test]
fn registration_identifies_each_immutable_definition_once() {
    let definition = crate::parse_definition(
        "function helper() -> Int\nkernel 1\ncharge 1\nbody { return 1 }\n",
    )
    .unwrap();
    let before = crate::function::IDENTITY_CALLS.with(std::cell::Cell::get);
    crate::FunctionRegistry::declarations()
        .register(definition, None)
        .unwrap();
    let after = crate::function::IDENTITY_CALLS.with(std::cell::Cell::get);
    assert_eq!(after - before, 1);
}

#[test]
fn plan_callback_membership_preserves_first_refusal_and_atomic_installation() {
    use crate::plan::{HelperPlan, PlanSite, SiteCall};

    let mut base = FunctionRegistry::new();
    let root = base.register(definition("law.root", false), None).unwrap();
    let first = base.register(definition("law.first", false), None).unwrap();
    let second = base
        .register(definition("law.second", false), None)
        .unwrap();
    let callees: &'static [_] = Box::leak(Box::new([first, second]));
    for (invoked, missing) in [
        (&[1, 1][..], Some(first)),
        (&[0, 0][..], Some(second)),
        (&[1, 0, 1][..], None),
    ] {
        let sites = invoked
            .iter()
            .map(|callee| PlanSite {
                path: &[],
                call: SiteCall::Invoke(*callee),
                dst: 0,
                roles: &[],
                bound: &[],
                loops: &[],
            })
            .collect::<Vec<_>>();
        let plan = Box::leak(Box::new(HelperPlan {
            name: "law.root",
            function: root,
            callees,
            literals: &[],
            fields: &[],
            records: &[],
            registers: 1,
            slots: &[],
            closures: &[],
            sites: Box::leak(sites.into_boxed_slice()),
            run: |_, _| Ok(crate::Value::Null),
            template: None,
        }));
        let mut registry = base.clone();
        let result = registry.install_plans(&[plan]);
        match missing {
            Some(expected) => {
                assert!(
                    matches!(result, Err(RegistryError::PlanCalleeUnanswered { callee, .. }) if callee == expected)
                );
                assert!(registry.get(&root).unwrap().plan().is_none());
            }
            None => {
                assert_eq!(result, Ok(1));
                assert!(registry.get(&root).unwrap().plan().is_some());
            }
        }
    }
}

/// K-DOC/L-START: Function Exports retain immutable definitions and need no Main.
#[test]
fn imports_and_function_exports_round_trip_without_main() {
    use crate::{
        Document, Export, FunctionExport, NumberPolicy, StartConvention, parse_document,
        print_document,
    };

    use lash_vocabulary::names::Name;
    let definition=parse_definition("function app.echo(value: Text) -> Text\nkernel 1\nmetering statements\nbody { return value }").unwrap();
    let mut document = Document::new(NumberPolicy::Float, vec![]);
    document.main = None;
    document.exports.insert(
        Name::new("echo"),
        Export::Function(FunctionExport {
            function: definition.identity(),
            signature: definition.signature.clone(),
            convention: StartConvention::Direct,
            asynchronous: false,
            rest: false,
        }),
    );
    document.functions.insert(definition.identity(), definition);
    crate::validate_document(&document, &document.functions).unwrap();
    assert_eq!(
        parse_document(&print_document(&document)).unwrap(),
        document
    );
    let encoded = lash_vocabulary::tokens::encode_body(&document);
    assert_eq!(
        lash_vocabulary::tokens::decode_body::<Document>(&encoded).unwrap(),
        document
    );
}
