pub const CONVENTIONS: lash_kernel_dialect::conventions::Conventions =
    lash_kernel_dialect::conventions::Conventions {
        inverse: lash_kernel_dialect::conventions::Inverse::Checked(verify),
        ..lash_dialect_conventions::python::CONVENTIONS
    };

fn verify(
    document: &lash_kernel_doc::Document,
    library: &dyn lash_kernel_dialect::Library,
) -> Result<lash_kernel_doc::Document, lash_kernel_dialect::PrintError> {
    let printed = crate::print(document, None, library, Default::default())?;
    lash_kernel_dialect::conventions::verify_lowering(document, library, |environment, mode| {
        crate::lower(&printed.source, environment, mode)
            .map(|lowered| lowered.document.into_document())
            .map_err(|diagnostic| diagnostic.to_string())
    })
}
pub(crate) use lash_dialect_conventions::python::{ITERATIONS, iteration};

pub(crate) use lash_dialect_conventions::python::REPLY;
