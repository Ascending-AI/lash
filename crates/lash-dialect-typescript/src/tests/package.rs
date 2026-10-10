use lash_kernel_dialect::{Library, define_functions};
use lash_kernel_doc::FunctionCatalog;

use crate::builtins::Receiver;
use crate::define_helpers;

/// Every helper source is kernel text that satisfies the statement rule and
/// calls only functions the library holds, and every name the lowerer
/// emits a call to is defined.
#[test]
fn the_helper_package_is_defined_against_the_kernel_library() {
    let mut library = lash_kernel_dialect::NamedLibrary::from_registry(&super::kernel_registry())
        .expect("unique kernel names");
    let definitions = match define_helpers(&mut library) {
        Ok(definitions) => definitions,
        Err(error) => panic!("{error}"),
    };
    assert!(definitions.len() > 40, "{} helpers", definitions.len());
    for name in crate::lower::CORE_OPERATIONS {
        assert!(library.resolve(name).is_some(), "`{name}` is not defined");
    }
}

/// A method name's dispatcher chooses a row by the receiver's kind and
/// falls back to calling the receiver's own member by that name.
#[test]
fn a_dispatcher_is_generated_from_a_names_rows() {
    let mut library = super::library().clone();
    let rows = [
        (Receiver::List, "ts.console.log"),
        (Receiver::Brand("RegExp"), "ts.console.log"),
    ];
    let source = crate::package::dispatcher(crate::builtins::table(), "method", "probe", &rows);
    let definitions = match define_functions(&source, &mut library) {
        Ok(definitions) => definitions,
        Err(error) => panic!("{error}\n{source}"),
    };
    let text = lash_kernel_doc::print_definition(&definitions[0]);
    assert!(
        text.starts_with("function ts.method.probe(this: Any, args: List(Any)) -> Any"),
        "{text}"
    );
    assert!(
        text.contains("if same(receiver, \"brand:RegExp\") {"),
        "{text}"
    );
    assert!(
        text.contains("let member = invoke ts.read(this, \"probe\")"),
        "{text}"
    );
    assert!(
        text.contains("let outcome = invoke ts.call_value(member, this, \"probe\", args)"),
        "{text}"
    );
    let fallback = library.resolve("ts.call_value").unwrap();
    let body = definitions[0].body().unwrap();
    assert!(body.functions.contains_key(&fallback));
    assert!(library.definition(&fallback).is_some());
}
