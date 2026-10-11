//! The registry used by the gate-six benchmark and its allocation law.

use std::sync::Arc;

use lash_kernel_dialect::NamedLibrary;
use lash_kernel_doc::FunctionRegistry;

#[expect(
    clippy::expect_used,
    reason = "benchmark setup: the shipped library and helpers register by construction"
)]
pub(crate) fn registry() -> Arc<FunctionRegistry> {
    let mut registry = FunctionRegistry::new();
    lash_kernel_lib::register_numbers(&mut registry).expect("numeric library registration");
    lash_kernel_lib::register_text_json(&mut registry).expect("text library registration");
    lash_kernel_vm::register_machine_functions(&mut registry)
        .expect("machine library registration");
    lash_kernel_lib::register_collections(&mut registry).expect("collection library registration");
    lash_ext_regex_ecma::register(
        &mut registry,
        &Arc::new(lash_ext_regex_ecma::Engine::new(32)),
    )
    .expect("regex extension registration");
    lash_ext_date_ecma::register(&mut registry).expect("date extension registration");
    lash_ext_url_whatwg::register(&mut registry).expect("URL extension registration");
    let mut library = NamedLibrary::from_registry(&registry).expect("unique kernel names");
    for definition in lash_dialect_typescript::define_helpers(&mut library)
        .unwrap_or_else(|error| panic!("{error}"))
    {
        registry
            .register(definition, None)
            .unwrap_or_else(|error| panic!("{error}"));
    }
    Arc::new(registry)
}
