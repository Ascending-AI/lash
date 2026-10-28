pub const CONVENTIONS: lash_kernel_dialect::conventions::Conventions =
    lash_kernel_dialect::conventions::Conventions {
        inverse: lash_kernel_dialect::conventions::Inverse::Checked(verify),
        separable_step,
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

fn separable_step(
    document: &lash_kernel_doc::Document,
    statement: &lash_kernel_value::sites::Site,
    catalog: &dyn lash_kernel_doc::FunctionCatalog,
) -> bool {
    let mut ancestor = statement.clone();
    while ancestor.path.pop().is_some() {
        let Some(lash_kernel_doc::Node::Block(block)) = document.node(&ancestor) else {
            continue;
        };
        for index in 0..block.len() {
            if super::collections::COMPREHENSION
                .recognize(&block[index..], &|id, name| {
                    catalog.named_helper(name) == Some(*id)
                })
                .is_some()
            {
                let body = ancestor.child(index as u32 + 1).child(1);
                if statement.unit == body.unit && statement.path.starts_with(&body.path) {
                    return false;
                }
            }
        }
    }
    true
}
