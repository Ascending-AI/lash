//! The laws of a function bundle (FIG-5887 §4).

use std::collections::BTreeMap;

use lash_kernel_doc::graph_tokens::{ChildRef, decode_body};
use lash_kernel_doc::{
    Action, Callee, Document, EffectName, Function, Name, NumberPolicy, Rhs, Signature, Stmt, Type,
};

use super::{Bundle, BundleBody, BundleRefusal, DOCUMENT_NODE_KIND, FUNCTION_BUNDLE_NODE_KIND};
use crate::SavedFunction;

/// A saved function `name` whose body calls each of `calls` by name.
fn function(name: &str, calls: &[&str]) -> SavedFunction {
    let mut document = Document::new(NumberPolicy::Float, Vec::new());
    document.functions.insert(
        Name::new(name),
        Function {
            params: Vec::new(),
            body: calls
                .iter()
                .map(|callee| Stmt::Let {
                    name: Name::new(format!("{callee}_called")),
                    value: Rhs::Action(Action::Call {
                        callee: Callee::Declared(Name::new(*callee)),
                        args: Vec::new(),
                    }),
                })
                .collect(),
        },
    );
    SavedFunction {
        name: Name::new(name),
        document,
        captures: BTreeMap::new(),
        written: None,
        token: None,
    }
}

/// A bundle holds its function and every saved function it uses, each once:
/// two functions that use one another make one bundle whose members are
/// both, and a function nothing it uses names stays out. Read back, the
/// bundle is the one staged, and a body whose required effects are not its
/// members' own, or whose member its document does not declare, is
/// refused.
#[test]
fn a_bundle_is_its_functions_closure_and_is_refused_unless_its_members_agree() {
    let functions: BTreeMap<Name, SavedFunction> = [
        function("even", &["odd"]),
        function("odd", &["even"]),
        function("apart", &[]),
    ]
    .into_iter()
    .map(|function| (function.name.clone(), function))
    .collect();
    let even = Bundle::of(&functions, &Name::new("even")).expect("a held function");
    let odd = Bundle::of(&functions, &Name::new("odd")).expect("a held function");
    assert_eq!(even, odd, "one member table for the mutual pair");
    assert_eq!(
        even.members().keys().map(Name::as_str).collect::<Vec<_>>(),
        ["even", "odd"]
    );
    assert_eq!(even.index(&Name::new("odd")), Some(1));

    let mut documents = BTreeMap::new();
    let release = ChildRef([7; 32]);
    let mut bodies = Vec::new();
    let mut staged = 0_u8;
    even.stage(release, |kind, logical| {
        staged += 1;
        let reference = ChildRef([staged; 32]);
        match kind {
            DOCUMENT_NODE_KIND => {
                documents.insert(
                    reference,
                    decode_body::<Document>(logical).expect("a document"),
                );
            }
            FUNCTION_BUNDLE_NODE_KIND => bodies.push(logical.to_vec()),
            other => panic!("unexpected kind {other}"),
        }
        Ok::<_, BundleRefusal>(reference)
    })
    .expect("stage");
    let body: BundleBody = decode_body(&bodies[0]).expect("a bundle body");
    let read = |body: BundleBody| {
        Bundle::assemble(body, |reference| {
            Ok::<_, BundleRefusal>(documents[&reference].clone())
        })
    };
    assert_eq!(read(body.clone()), Ok((even, release)));

    let mut effects = body.clone();
    effects.effects.push((
        EffectName::new("echo").expect("an effect name"),
        Signature {
            params: Vec::new(),
            result: Type::Any,
        },
    ));
    assert_eq!(read(effects), Err(BundleRefusal::RequiredEffects));

    let mut renamed = body;
    renamed.members[0].name = Name::new("evens");
    assert_eq!(
        read(renamed),
        Err(BundleRefusal::Undeclared {
            member: Name::new("evens")
        })
    );
}
