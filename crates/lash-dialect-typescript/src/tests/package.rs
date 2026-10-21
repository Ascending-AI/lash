use crate::define_helpers;

/// Every helper source is kernel text that structural validation admits and
/// calls only functions the library holds.
#[test]
fn the_helper_package_is_defined_against_the_kernel_library() {
    let registry = lash_ext_helper_compiler::library::Library::of(
        lash_ext_helper_compiler::library::Dialect::TypeScript,
    )
    .unwrap()
    .base_registry()
    .unwrap();
    let mut library = lash_kernel_dialect::NamedLibrary::from_registry(&registry).unwrap();
    define_helpers(&mut library).unwrap_or_else(|error| panic!("{error}"));
}
