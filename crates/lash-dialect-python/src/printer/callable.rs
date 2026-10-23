//! A selected callable declaration, with the original function Sites.
use super::*;
pub(crate) fn print_callable(
    base: &lash_kernel_dialect::CallableSourceBase,
    library: &dyn lash_kernel_dialect::Library,
    options: PrintOptions,
) -> Result<PrintedSource, PrintError> {
    let original = base
        .document
        .to_document()
        .map_err(|e| PrintError::Invalid(e.to_string()))?;
    let mut document = original.clone();
    document.exports.clear();
    document.main = Some(vec![k::Stmt::Let {
        name: lash_kernel_dialect::callable::entry_name(base, legal_identifier)?,
        value: k::Rhs::Expr(k::Expr::Closure(Box::new(k::Closure {
            function: base.function.callable.function,
            presentation: Some(k::CallablePresentation {
                signature: base.function.signature.clone(),
                convention: base.function.convention.clone(),
                representation: base.function.representation.clone(),
            }),
        }))),
    }]);
    let mut entries =
        base.function
            .captions
            .entries()
            .iter()
            .filter(|entry| {
                entry.target.site().is_some_and(|site| {
                    matches!(site.unit, lash_vocabulary::sites::Unit::Library(_))
                })
            })
            .cloned()
            .collect::<Vec<_>>();
    if let Some(caption) = base
        .function
        .captions
        .at(&lash_vocabulary::sites::Site::new(
            lash_vocabulary::sites::Unit::Library(base.function.callable.function),
            [],
        ))
    {
        entries.push(k::CaptionEntry {
            target: k::CaptionTarget::Site(lash_vocabulary::sites::Site::new(
                lash_vocabulary::sites::Unit::Main,
                [0],
            )),
            caption: caption.clone(),
        });
    }
    let captions =
        k::Captions::new(&document, entries).map_err(|e| PrintError::Invalid(e.to_string()))?;
    let regions = Regions {
        authored: BTreeSet::new(),
        whole: vec![lash_kernel_dialect::regions::ModulePart::Statements(0..1)],
    };
    let options = PrintOptions {
        source_map: false,
        ..options
    };
    // External direct calls carry their await policy in the code. A bare
    // call must not acquire an extra await because its declaration is frozen.
    let (printed, _) = print_regions(
        &document,
        library,
        Some(&captions),
        options,
        &regions,
        &base.function.captures.keys().cloned().collect(),
    )?;
    Ok(printed)
}
