use lash_kernel_dialect::Library;

use crate::define_helpers;

/// Every helper source is kernel text that structural validation admits and
/// calls only functions the library holds, and every name the lowerer
/// emits a call to is defined.
#[test]
fn the_helper_package_is_defined_against_the_kernel_library() {
    let mut library =
        lash_kernel_dialect::NamedLibrary::resolving(super::machine::registry(), |id| {
            super::machine::registry()
                .get(id)
                .is_some_and(|function| !function.name().starts_with("ts."))
        })
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
