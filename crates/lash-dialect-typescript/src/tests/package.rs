use crate::define_helpers;

/// Every helper source is kernel text that structural validation admits and
/// calls only functions the library holds.
#[test]
fn the_helper_package_is_defined_against_the_kernel_library() {
    let mut library =
        lash_kernel_dialect::NamedLibrary::resolving(super::machine::registry(), |id| {
            super::machine::registry()
                .get(id)
                .is_some_and(|function| !function.name().starts_with("ts."))
        })
        .expect("unique kernel names");
    define_helpers(&mut library).unwrap_or_else(|error| panic!("{error}"));
}
