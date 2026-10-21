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
