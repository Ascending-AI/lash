//! The property names whose ECMA meaning is the prototype chain.
//!
//! The value model is dense records with no prototypes, so none of these names
//! has anything to read or mutate. Accepting them would not be a small
//! divergence: `o.__proto__ = base` would land as an ordinary data key, every
//! later read through the chain would miss, and `__defineGetter__` would install
//! nothing while reporting success. The census has claimed
//! `TS_PROTOTYPE_MUTATION_UNSUPPORTED` for this family since it was written;
//! this module is what makes the claim true.

use super::{Diagnostic, DiagnosticCode, reject, source_span};
use swc_common::Spanned;
use swc_ecma_ast as swc;

pub(super) fn is_prototype_chain_property(name: &str) -> bool {
    matches!(
        name,
        "prototype"
            | "__proto__"
            | "__defineGetter__"
            | "__defineSetter__"
            | "__lookupGetter__"
            | "__lookupSetter__"
    )
}

pub(super) fn prototype_access_rejection(span: swc_common::Span) -> Diagnostic {
    reject(
        DiagnosticCode::PrototypeMutationUnsupported,
        "prototype access",
        Some(source_span(span)),
    )
}

/// Rejects a literal `__proto__:` key in an object literal.
///
/// A literal `__proto__` key is not a data property in ECMA — it sets the
/// prototype. A computed `[key]` with the same name *is* data, which is why
/// only the literal forms reject here; the computed one is answered at the
/// write, where the name is first known.
pub(super) fn check_property_key(name: &swc::PropName) -> Result<(), Diagnostic> {
    let literal = match name {
        swc::PropName::Ident(key) => Some(key.sym.to_string()),
        swc::PropName::Str(key) => Some(key.value.to_string_lossy().into_owned()),
        _ => None,
    };
    match literal {
        Some(key) if key == "__proto__" => Err(prototype_access_rejection(name.span())),
        _ => Ok(()),
    }
}
