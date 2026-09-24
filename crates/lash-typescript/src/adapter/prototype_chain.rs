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

pub(crate) fn is_prototype_chain_property(name: &str) -> bool {
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

/// Whether `expr` is `<built-in global>.prototype` — the member access the
/// adapter admits for reads, but never as a mutation target:
/// `Array.prototype[1] = x` would land on the prototype object while staying
/// invisible to `[]`'s element reads, a silent near-miss.
pub(super) fn is_builtin_prototype_object(expr: &swc::Expr) -> bool {
    let swc::Expr::Member(member) = expr else {
        return false;
    };
    let swc::Expr::Ident(owner) = member.obj.as_ref() else {
        return false;
    };
    if !lashlang::is_javascript_builtin_global(owner.sym.as_ref()) {
        return false;
    }
    match &member.prop {
        swc::MemberProp::Ident(name) => name.sym.as_ref() == "prototype",
        swc::MemberProp::Computed(computed) => matches!(
            computed.expr.as_ref(),
            swc::Expr::Lit(swc::Lit::Str(name)) if name.value.to_string_lossy() == "prototype"
        ),
        _ => false,
    }
}

/// The refusal for a member write whose object is `<built-in global>.prototype`.
pub(super) fn builtin_prototype_mutation(member: &swc::MemberExpr) -> Option<Diagnostic> {
    is_builtin_prototype_object(&member.obj).then(|| prototype_access_rejection(member.span))
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
