//! Laws of the linked document checker.

use lash_kernel_doc::{
    Document, Export, FunctionExport, NumberPolicy, StartConvention, parse_definition_with_locals,
};

use lash_kernel_value::names::Name;
use lash_kernel_value::sites::Unit;
/// K-CLO-001/K-SITE: anonymous bodies resolve lexical captures and retain their defining sites.
#[test]
fn anonymous_definition_keeps_lexical_capture_and_its_own_unit() {
    let (root, locals) = parse_definition_with_locals("function app.make(value: Any) -> Any\nkernel 1\nmetering statements\nbody { let f = fn() { return value } return f }").unwrap();
    let anonymous = locals[0].identity();
    let mut document = Document::new(NumberPolicy::Float, vec![]);
    document.main = None;
    document.exports.insert(
        Name::new("make"),
        Export::Function(FunctionExport {
            function: root.identity(),
            signature: root.signature.clone(),
            convention: StartConvention::Direct,
            asynchronous: false,
            rest: false,
        }),
    );
    document.functions.insert(root.identity(), root);
    document
        .functions
        .extend(locals.into_iter().map(|f| (f.identity(), f)));
    let graph = crate::derive(&document, &document.functions).unwrap();
    assert!(
        graph
            .nodes()
            .iter()
            .any(|node| node.site.unit == Unit::Library(anonymous))
    );
    assert!(
        graph
            .bindings()
            .iter()
            .any(|binding| binding.name == Name::new("value") && binding.captured)
    );
}

/// FIG-7143: entry admission requires annotations; ordinary functions stay free.
#[test]
fn incomplete_entry_requires_signature_without_restricting_ordinary_functions() {
    let (definition, _) = parse_definition_with_locals("function app.free(value: Any) -> Any\nkernel 1\nmetering statements\nbody { return value }").unwrap();
    let mut document = Document::new(NumberPolicy::Float, vec![]);
    let function = definition.identity();
    let signature = definition.signature.clone();
    document.functions.insert(function, definition);
    crate::derive(&document, &document.functions)
        .expect("ordinary unannotated functions remain valid");
    document.exports.insert(
        Name::new("entry"),
        Export::Function(FunctionExport {
            function,
            signature,
            convention: StartConvention::Direct,
            asynchronous: false,
            rest: false,
        }),
    );
    document.main = Some(vec![lash_kernel_doc::Stmt::Return {
        value: lash_kernel_doc::Expr::Closure(Box::new(lash_kernel_doc::Closure {
            function,
            presentation: None,
        })),
    }]);
    let captured = lash_kernel_doc::FunctionValue {
        written_signature: None,
        signature_origin: None,
        implicit_text_conversions: Vec::new(),
        callable: lash_kernel_value::CallableId {
            link: lash_kernel_value::LinkId::from_bytes([1; 32]),
            function,
        },
        captures: Default::default(),
        signature: document.functions[&function].signature.clone(),
        convention: StartConvention::Direct,
        captions: lash_kernel_doc::Captions::empty(document.identity()),
        representation: lash_kernel_doc::FunctionRepresentation::Bare,
        creation: Some(lash_kernel_value::sites::Site::new(Unit::Main, [0, 0])),
        token: None,
    };
    captured
        .check_definition(
            &document,
            &lash_kernel_doc::BuildHelpers::empty(),
            &lash_kernel_doc::Selection::Fresh,
        )
        .expect("an ordinary captured kernel closure needs no dialect presentation");
    assert!(matches!(
        captured.entry_export(
            &document,
            &lash_kernel_doc::BuildHelpers::empty(),
            &lash_kernel_doc::Selection::Fresh
        ),
        Err(lash_kernel_doc::FunctionValueRefusal::SignatureRequired { .. })
    ));
    let refusal = crate::startable_export(&document, &Name::new("entry")).unwrap_err();
    let crate::StartRefusal::SignatureRequired(required) = refusal else {
        panic!("expected typed signature requirement: {refusal}")
    };
    assert_eq!(
        required.position,
        lash_kernel_doc::SignaturePosition::Parameter {
            index: 0,
            name: Name::new("value")
        }
    );
    assert_eq!(
        serde_json::to_value(&required).unwrap()["code"],
        "SIGNATURE_REQUIRED"
    );
    let presentation = lash_kernel_doc::CallablePresentation {
        signature: lash_kernel_value::Signature {
            params: vec![lash_kernel_value::Param {
                name: Name::new("value"),
                ty: lash_kernel_value::Type::Json,
                optional: false,
            }],
            result: lash_kernel_value::Type::Any,
        },
        representation: lash_kernel_doc::FunctionRepresentation::Bare,
        convention: StartConvention::Direct,
    };
    let location = lash_kernel_doc::FunctionLocation {
        name: Some("entry".into()),
        site: None,
    };
    assert_eq!(
        presentation
            .promote_entry(function, location.clone(), false, false, None)
            .unwrap_err(),
        lash_kernel_value::EntrySignatureRefusal::Required(Box::new(
            lash_kernel_value::SignatureRequired {
                function: location.clone(),
                position: lash_kernel_doc::SignaturePosition::Result,
                span: None
            }
        ))
    );
    let complete = lash_kernel_doc::CallablePresentation {
        signature: lash_kernel_value::Signature {
            result: lash_kernel_value::Type::Json,
            ..presentation.signature
        },
        ..presentation
    };
    assert_eq!(
        complete
            .promote_entry(function, location, false, false, None)
            .unwrap()
            .signature,
        complete.signature
    );
}

/// FIG-7143: transported metadata cannot replace the defining public signature.
#[test]
fn entry_promotion_verifies_the_defining_callable_contract() {
    let definition = lash_kernel_doc::parse_definition(
        "function app.entry() -> Bool\nkernel 1\nmetering statements\nbody { return true }",
    )
    .unwrap();
    let id = definition.identity();
    let mut document = Document::new(NumberPolicy::Float, vec![]);
    document.functions.insert(id, definition);
    let mut value = lash_kernel_doc::FunctionValue {
        written_signature: None,
        signature_origin: None,
        implicit_text_conversions: Vec::new(),
        callable: lash_kernel_value::CallableId {
            link: lash_kernel_value::LinkId::from_bytes([1; 32]),
            function: id,
        },
        captures: Default::default(),
        signature: lash_kernel_value::Signature {
            params: vec![],
            result: lash_kernel_value::Type::Bool,
        },
        convention: StartConvention::Direct,
        captions: lash_kernel_doc::Captions::empty(document.identity()),
        representation: lash_kernel_doc::FunctionRepresentation::Bare,
        creation: None,
        token: None,
    };
    assert_eq!(
        value
            .entry_export(
                &document,
                &lash_kernel_doc::BuildHelpers::empty(),
                &lash_kernel_doc::Selection::Fresh
            )
            .unwrap()
            .signature,
        value.signature
    );
    value.signature.result = lash_kernel_value::Type::Text;
    assert_eq!(
        value
            .entry_export(
                &document,
                &lash_kernel_doc::BuildHelpers::empty(),
                &lash_kernel_doc::Selection::Fresh
            )
            .unwrap_err(),
        lash_kernel_doc::FunctionValueRefusal::Identity
    );
}

/// K-CALLABLE-RESTORE: a checked captured view never grants helper selection to a fresh read.
#[test]
fn a_captured_view_does_not_grant_retired_helper_authority() {
    use lash_kernel_doc::{
        BuildHelpers, Captions, FunctionRegistry, FunctionRepresentation, FunctionValue,
        Implementation, Selection,
    };
    let helper = lash_kernel_doc::parse_definition(
        "function law.helper(x: Int) -> Int\nkernel 1\ncharge 1\nbody {return x}",
    )
    .unwrap();
    let helper_id = helper.identity();
    let mut root = lash_kernel_doc::parse_definition(
        "function app.run(x: Int) -> Int\nkernel 1\ncharge 1\nbody {return x}",
    )
    .unwrap();
    let Implementation::Guest { body, .. } = &mut root.implementation else {
        panic!("guest body")
    };
    body.functions.insert(helper_id, helper.name.clone());
    body.block = vec![lash_kernel_doc::Stmt::Return {
        value: lash_kernel_doc::Expr::Act(Box::new(lash_kernel_doc::ActExpr {
            kind: lash_kernel_doc::ActKind::Call(lash_kernel_doc::Callee::Local(helper_id)),
            args: vec![lash_kernel_doc::Expr::Variable(Name::new("x"))],
        })),
    }];
    let root_id = root.identity();
    let mut registry = FunctionRegistry::declarations();
    registry.register(helper, None).unwrap();
    let helpers = BuildHelpers::new(registry, [helper_id]).unwrap();
    let mut document = Document::new(NumberPolicy::Float, vec![]);
    document.library.insert(helper_id);
    let signature = root.signature.clone();
    document.functions.insert(root_id, root);
    let function = FunctionValue {
        written_signature: None,
        signature_origin: None,
        implicit_text_conversions: vec![],
        callable: lash_kernel_value::CallableId {
            link: lash_kernel_value::LinkId::from_bytes([1; 32]),
            function: root_id,
        },
        signature,
        convention: StartConvention::Direct,
        representation: FunctionRepresentation::Bare,
        captures: Default::default(),
        captions: Captions::empty(document.identity()),
        creation: None,
        token: None,
    };
    let viewed =
        crate::WorkflowDocument::captured(document.clone(), function.clone(), &helpers).unwrap();
    assert_eq!(viewed.callable(), Some(&function));
    assert!(
        crate::WorkflowDocument::captured(
            document.clone(),
            function.clone(),
            &BuildHelpers::empty()
        )
        .is_err()
    );
    assert!(!Selection::Fresh.admits_helper(
        &lash_kernel_value::CallableId {
            link: function.callable.link,
            function: helper_id
        },
        &document.library,
        &BuildHelpers::empty()
    ));
}
