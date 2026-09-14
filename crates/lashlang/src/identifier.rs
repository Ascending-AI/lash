//! The identifier shape a process parameter name has to have.
//!
//! `ProcessSignature` is a durable surface: a parameter name travels in the
//! module artifact and is bound by name at every call. The rule is therefore
//! stated here rather than borrowed from a front-end's grammar — ADR 0096
//! leaves the IR with no grammar of its own, and a name that linked yesterday
//! must not stop linking because a dialect changed its keyword list.

/// The names the retired Lashlang lexer could never produce as an identifier.
///
/// They are kept as a refusal rather than dropped with the grammar: every
/// artifact published so far was validated against them, so widening the rule
/// now would accept parameter names that older readers reject.
const RESERVED_PARAMETER_NAMES: &[&str] = &[
    "if", "else", "for", "in", "await", "cancel", "submit", "print", "call", "and", "or", "not",
    "true", "false", "null",
];

/// Whether `name` is a plain ASCII identifier — a leading letter or underscore,
/// then letters, digits or underscores — that is not a reserved name.
pub(crate) fn is_process_parameter_name(name: &str) -> bool {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    (first == '_' || first.is_ascii_alphabetic())
        && chars.all(|ch| ch == '_' || ch.is_ascii_alphanumeric())
        && !RESERVED_PARAMETER_NAMES.contains(&name)
}
