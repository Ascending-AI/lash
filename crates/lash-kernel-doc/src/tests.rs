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

/// FIG-7143 / K-EFF-005: value paths name fields and array positions; limits
/// never manufacture a type mismatch, even within a union.
#[test]
fn decode_mismatch_retains_value_path_and_depth_is_a_limit() {
    use lash_vocabulary::{RecordType, RecordTypeField, Type, TypeMismatch};
    let expected = Type::Record(RecordType {
        fields: vec![RecordTypeField {
            name: "x/y~z".into(),
            ty: Type::List(Box::new(Type::Text)),
            optional: false,
        }],
        rest: None,
    });
    let value = crate::Datum::Record(vec![(
        "x/y~z".into(),
        crate::Datum::List(vec![crate::Datum::Bool(true)]),
    )]);
    let decoder = crate::Decoder {
        max_depth: 64,
        policy: crate::NumberPolicy::Float,
        declared: &|_| false,
    };
    assert_eq!(
        decoder.decode(&value, &expected),
        Err(crate::DecodeRefusal::Type {
            mismatch: TypeMismatch::new(Type::Text, Type::Bool)
                .within("0")
                .within("x/y~z")
        })
    );
    let map = Type::Map(Box::new(lash_vocabulary::MapType {
        key: Type::Text,
        value: Type::List(Box::new(Type::Text)),
    }));
    assert_eq!(
        decoder.decode(&value, &map),
        Err(crate::DecodeRefusal::Type {
            mismatch: TypeMismatch::new(Type::Text, Type::Bool)
                .within("0")
                .within("x/y~z")
        })
    );
    let limited = crate::Decoder {
        max_depth: 1,
        ..decoder
    };
    assert_eq!(
        limited.decode(&value, &Type::Union(vec![expected, Type::Json])),
        Err(crate::DecodeRefusal::DepthLimit { limit: 1 })
    );
}

/// FIG-7143 / K-EFF-005: unconstrained data still names the offending
/// map member or error payload, rather than reporting its enclosing value.
#[test]
fn unconstrained_decode_mismatches_keep_map_and_error_value_paths() {
    use crate::{Datum, DecodeRefusal, Decoder, ErrorDatum, NumberPolicy, NumberToken};
    use lash_vocabulary::{Type, TypeMismatch};
    let decoder = Decoder {
        max_depth: 64,
        policy: NumberPolicy::Float,
        declared: &|_| false,
    };
    let invalid = Datum::Number(NumberToken::new("1e999").unwrap());
    let error = |data| {
        Datum::Error(Box::new(ErrorDatum {
            kind: "example".into(),
            message: String::new(),
            data,
        }))
    };
    for ty in [Type::Any, Type::Json] {
        for (value, path) in [
            (
                Datum::Map(vec![(Datum::Text("x/y~z".into()), invalid.clone())]),
                "/x~1y~0z",
            ),
            (
                Datum::Map(vec![(Datum::Bool(true), invalid.clone())]),
                "/0/1",
            ),
            (Datum::Map(vec![(invalid.clone(), Datum::Null)]), "/0/0"),
            (error(invalid.clone()), "/data"),
        ] {
            assert_eq!(
                decoder.decode(&value, &ty),
                Err(DecodeRefusal::Type {
                    mismatch: TypeMismatch {
                        path: path.into(),
                        expected: Type::Float,
                        actual: Type::Number,
                    },
                }),
                "{ty:?} at {path}"
            );
        }
    }
    assert_eq!(
        decoder.decode(&error(invalid), &Type::Error),
        Err(DecodeRefusal::Type {
            mismatch: TypeMismatch::new(Type::Float, Type::Number).within("data"),
        })
    );
}

/// Explicit library origin admits an exact reference without copying its body;
/// an ambient registry entry alone never authorizes that reference.
#[test]
fn library_references_require_an_explicit_disjoint_manifest() {
    use crate::{ActExpr, ActKind, Callee, Document, Expr, NumberPolicy, Stmt};
    let helper = definition("law.helper", false);
    let function = helper.identity();
    let mut registry = FunctionRegistry::declarations();
    registry.register(helper.clone(), None).unwrap();
    let mut document = Document::new(
        NumberPolicy::BySpelling,
        vec![Stmt::Print {
            value: Expr::Act(Box::new(ActExpr {
                kind: ActKind::Call(Callee::Local(function)),
                args: vec![Expr::Literal(crate::Literal::Null)],
            })),
        }],
    );
    assert!(crate::validate_document(&document, &registry).is_err());
    document.library.insert(function);
    crate::validate_document_structure(&document)
        .expect("stored declared library refs need no installed helper");
    crate::validate_document(&document, &registry).expect("explicit build reference is admitted");
    document.functions.insert(function, helper);
    assert!(
        crate::validate_document(&document, &registry).is_err(),
        "origins must be disjoint"
    );
}

/// Release identity is the closed helper universe, independent of root order,
/// duplicate roots and unrelated host functions; changing a member changes it.
#[test]
fn helper_library_identity_covers_only_the_closed_build_universe() {
    use crate::{BuildHelpers, FunctionBody, Implementation};
    let leaf = definition("law.leaf", false);
    let leaf_id = leaf.identity();
    let mut root = definition("law.root", false);
    if let Implementation::Guest {
        body: FunctionBody { functions, .. },
        ..
    } = &mut root.implementation
    {
        functions.insert(leaf_id, leaf.name.clone());
    }
    let root_id = root.identity();
    let mut registry = FunctionRegistry::declarations();
    registry.register(leaf.clone(), None).unwrap();
    registry.register(root, None).unwrap();
    let first = BuildHelpers::new(registry.clone(), [root_id]).unwrap();
    registry
        .register(definition("host.unrelated", false), None)
        .unwrap();
    let second = BuildHelpers::new(registry, [leaf_id, root_id, root_id]).unwrap();
    assert_eq!(first.identity(), second.identity());
    assert_eq!(
        first
            .members()
            .copied()
            .collect::<std::collections::BTreeSet<_>>(),
        [leaf_id, root_id].into()
    );
    assert_ne!(
        first.identity(),
        BuildHelpers::new(first.functions().clone(), [leaf_id])
            .unwrap()
            .identity()
    );
    assert_eq!(
        lash_vocabulary::HelperLibraryId::of_members([root_id, leaf_id]),
        first.identity()
    );
    let _ = crate::document_work::helper_work();
    let reopened = BuildHelpers::from_frame(
        first.identity(),
        first.functions().clone(),
        first.members().copied().collect(),
    )
    .unwrap();
    assert!(reopened.contains(&root_id));
    assert_eq!(
        crate::document_work::helper_work(),
        crate::document_work::HelperWork::default()
    );
}

/// Live static reach includes a callable retained as an action's literal
/// argument, including the atom-only action forms that have no expression child.
#[test]
fn action_atom_function_literals_belong_to_static_reach() {
    use crate::{Action, Atom, Callee, FunctionTarget, Literal, Node};
    use lash_vocabulary::{FunctionId, ImportSlot, Name};
    let function = FunctionId::from_bytes([9; 32]);
    let atom = Atom::Literal(Literal::Function(function));
    let callee = Callee::Value(Name::new("callback"));
    let actions = [
        Action::Call {
            callee: callee.clone(),
            args: vec![atom.clone()],
        },
        Action::Spawn {
            callee,
            args: vec![atom.clone()],
        },
        Action::Perform {
            slot: ImportSlot(0),
            args: vec![atom.clone()],
            result: lash_vocabulary::Type::Any,
            then_yield: false,
        },
        Action::Await {
            policy: FunctionTarget::Import(ImportSlot(1)),
            value: atom.clone(),
        },
        Action::Join { task: atom.clone() },
        Action::JoinMany {
            tasks: atom.clone(),
            mode: crate::JoinMode::All,
        },
        Action::Cancel { task: atom },
    ];
    for action in actions {
        assert_eq!(
            Node::Action(&action).functions_within(),
            [function].into(),
            "{action:?}"
        );
    }
}

/// A returned helper closure preserves its owning FunctionId and creation site;
/// neither global membership nor a matching function name invents its origin.
#[test]
fn saved_helper_callable_preserves_its_exact_creation_origin() {
    use crate::{
        BuildHelpers, Captions, Document, FunctionRepresentation, FunctionValue, NumberPolicy,
        StartConvention,
    };
    use lash_vocabulary::sites::{Site, Unit};
    use lash_vocabulary::{CallableId, LinkId};
    let (owner, locals) = crate::parse_definition_with_locals(
        "function law.owner() -> Any\nkernel 1\ncharge 1\nbody { return fn(x) { return x } }",
    )
    .unwrap();
    let owner_id = owner.identity();
    let mut registry = FunctionRegistry::declarations();
    for local in locals {
        registry.register(local, None).unwrap();
    }
    registry.register(owner, None).unwrap();
    let helpers = BuildHelpers::new(registry, [owner_id]).unwrap();
    let mut document = Document::new(NumberPolicy::BySpelling, vec![]);
    document.library.insert(owner_id);
    let creation = Site::new(Unit::Library(owner_id), [0, 0]);
    let closure = crate::snapshot::closure_at(
        &document,
        &creation,
        LinkId::from_bytes([3; 32]),
        &helpers,
        &crate::Selection::Fresh,
    )
    .unwrap();
    let value = FunctionValue {
        callable: CallableId {
            link: LinkId::from_bytes([3; 32]),
            function: closure.function,
        },
        captures: Default::default(),
        signature: helpers
            .registered(&closure.function)
            .unwrap()
            .header()
            .signature
            .clone(),
        convention: StartConvention::Direct,
        representation: FunctionRepresentation::Bare,
        creation: Some(creation),
        captions: Captions::empty(document.identity()),
        token: None,
        signature_origin: None,
    };
    value
        .check_definition(&document, &helpers, &crate::Selection::Fresh)
        .unwrap();
    document.library.0.clear();
    assert_eq!(
        value.check_definition(&document, &helpers, &crate::Selection::Fresh),
        Err(crate::FunctionValueRefusal::Identity)
    );
}

/// A saved child remains admitted after its old parent disappears: restored
/// origin is the exact live callable and pin, rather than current ancestry.
#[test]
fn saved_child_of_removed_helper_keeps_only_its_live_pin() {
    use crate::{
        BuildHelpers, Captions, Document, FunctionRepresentation, FunctionValue,
        LibraryDependencies, MissingHelper, NumberPolicy, RestoreSelection, Selection,
        StartConvention,
    };
    use lash_vocabulary::{CallableId, LinkId};
    let child = definition("law.saved_child", false);
    let child_id = child.identity();
    let mut parent = definition("law.removed_parent", false);
    if let crate::Implementation::Guest { body, .. } = &mut parent.implementation {
        body.functions.insert(child_id, child.name.clone());
    }
    let parent_id = parent.identity();
    let mut registry = FunctionRegistry::declarations();
    registry.register(child.clone(), None).unwrap();
    let helpers = BuildHelpers::new(registry, [child_id]).unwrap();
    assert!(!helpers.contains(&parent_id));
    let mut document = Document::new(NumberPolicy::BySpelling, vec![]);
    document.library.insert(parent_id);
    let callable = CallableId {
        link: LinkId::from_bytes([7; 32]),
        function: child_id,
    };
    let selection = Selection::Restored(RestoreSelection {
        pins: [child_id].into(),
        live: [callable].into(),
    });
    let value = FunctionValue {
        callable,
        captures: Default::default(),
        signature: child.signature,
        convention: StartConvention::Direct,
        representation: FunctionRepresentation::Bare,
        creation: None,
        captions: Captions::empty(document.identity()),
        token: None,
        signature_origin: None,
    };
    assert!(!Selection::Fresh.admits_helper(&callable, &document.library, &helpers));
    assert!(selection.admits_helper(&callable, &document.library, &helpers));
    value
        .check_definition(&document, &helpers, &selection)
        .unwrap();
    let absent = BuildHelpers::empty();
    assert!(!selection.admits_helper(&callable, &document.library, &absent));
    assert_eq!(
        absent.reach(&LibraryDependencies([child_id].into())),
        Err(MissingHelper { function: child_id })
    );
    assert!(
        value
            .check_definition(&document, &absent, &selection)
            .is_err()
    );
}
