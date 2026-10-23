//! Laws of the linked document checker.

use lash_kernel_doc::{
    Document, Export, FunctionExport, NumberPolicy, StartConvention, parse_definition_with_locals,
};

use lash_vocabulary::names::Name;
use lash_vocabulary::sites::Unit;
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
        callable: lash_vocabulary::CallableId {
            link: lash_vocabulary::LinkId::from_bytes([1; 32]),
            function,
        },
        captures: Default::default(),
        signature: document.functions[&function].signature.clone(),
        convention: StartConvention::Direct,
        captions: lash_kernel_doc::Captions::empty(document.identity()),
        representation: lash_kernel_doc::FunctionRepresentation::Bare,
        creation: Some(lash_vocabulary::sites::Site::new(Unit::Main, [0, 0])),
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
        signature: lash_vocabulary::Signature {
            params: vec![lash_vocabulary::Param {
                name: Name::new("value"),
                ty: lash_vocabulary::Type::Json,
                optional: false,
            }],
            result: lash_vocabulary::Type::Any,
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
        lash_vocabulary::EntrySignatureRefusal::Required(Box::new(
            lash_vocabulary::SignatureRequired {
                function: location.clone(),
                position: lash_kernel_doc::SignaturePosition::Result,
                span: None
            }
        ))
    );
    let complete = lash_kernel_doc::CallablePresentation {
        signature: lash_vocabulary::Signature {
            result: lash_vocabulary::Type::Json,
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
        callable: lash_vocabulary::CallableId {
            link: lash_vocabulary::LinkId::from_bytes([1; 32]),
            function: id,
        },
        captures: Default::default(),
        signature: lash_vocabulary::Signature {
            params: vec![],
            result: lash_vocabulary::Type::Bool,
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
    value.signature.result = lash_vocabulary::Type::Text;
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
