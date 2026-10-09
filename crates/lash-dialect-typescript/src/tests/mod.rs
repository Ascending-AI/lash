use std::collections::{BTreeMap, BTreeSet};
use std::sync::OnceLock;

use lash_kernel_dialect::{Environment, Lowered, NamedLibrary};
use lash_kernel_doc::{
    EffectName, Signature, parse_document, print_document, validate_annotations, validate_document,
};

use crate::{Diagnostic, define_helpers, provisional};

mod async_fn;
mod deviations;
mod hoisting;
mod language;
mod machine;
mod package;
mod typed;

/// The stand-in kernel library with the dialect's helpers defined in it.
pub(crate) fn library() -> &'static NamedLibrary {
    static LIBRARY: OnceLock<NamedLibrary> = OnceLock::new();
    LIBRARY.get_or_init(|| {
        let mut library = provisional::kernel_library();
        if let Err(error) = define_helpers(&mut library) {
            panic!("{error}");
        }
        library
    })
}

/// Lowers a first cell and checks what every lowered document must hold:
/// it is admitted, its annotations are its own, and it survives kernel
/// text.
pub(crate) fn lower(source: &str) -> Result<Lowered, Diagnostic> {
    lower_in_session(source, &[])
}

pub(crate) fn lower_in_session(source: &str, bindings: &[&str]) -> Result<Lowered, Diagnostic> {
    lower_against(source, bindings, &BTreeMap::new())
}

/// Lowers a first cell whose host supplies the laws' tools, `echo` and
/// `boom`.
pub(crate) fn lower_with_effects(source: &str) -> Result<Lowered, Diagnostic> {
    lower_against(source, &[], &machine::effects())
}

fn lower_against(
    source: &str,
    bindings: &[&str],
    effects: &BTreeMap<EffectName, Signature>,
) -> Result<Lowered, Diagnostic> {
    let bindings: BTreeSet<_> = bindings.iter().map(|name| (*name).into()).collect();
    let environment = Environment {
        library: library(),
        effects,
        bindings: &bindings,
    };
    let lowered = crate::lower(source, &environment)?;
    if let Err(invalid) = validate_document(&lowered.document, library()) {
        panic!("{invalid}\n{}", print_document(&lowered.document));
    }
    if let Err(invalid) = validate_annotations(&lowered.annotations, &lowered.document) {
        panic!("{invalid}");
    }
    let text = print_document(&lowered.document);
    assert_eq!(
        parse_document(&text).as_ref(),
        Ok(&lowered.document),
        "{text}"
    );
    Ok(lowered)
}

/// The body of `main` in kernel text, one statement per line, without the
/// header that lists library functions.
pub(crate) fn main_text(source: &str) -> String {
    main_of(lower(source))
}

/// [`main_text`] of a cell that may call the laws' tools.
pub(crate) fn main_text_with_effects(source: &str) -> String {
    main_of(lower_with_effects(source))
}

fn main_of(lowered: Result<Lowered, Diagnostic>) -> String {
    let lowered = match lowered {
        Ok(lowered) => lowered,
        Err(diagnostic) => panic!("{diagnostic}"),
    };
    let text = print_document(&lowered.document);
    let start = text.find("main {").expect("a document has a main");
    text[start..]
        .lines()
        .skip(1)
        .take_while(|line| *line != "}")
        .map(|line| line.strip_prefix("  ").unwrap_or(line))
        .collect::<Vec<_>>()
        .join("\n")
}
