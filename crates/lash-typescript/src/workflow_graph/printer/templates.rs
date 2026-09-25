//! The template literal's shape in the lowered IR: the quasi/hole chain it
//! lowers to, and the raw-text escapes a quasi's spelling needs.

use lashlang::{Expr, JavaScriptBinaryOp};

/// A template literal's text and holes, from the chain it lowers to:
/// `q0 + e0 + q1 + … + en + qn+1`, left-nested, a string at every even
/// position. Printed back as the template, the chain lowers identically, and
/// its nesting costs one level rather than one per term.
pub(super) fn template_parts(expression: &Expr) -> Option<(Vec<&str>, Vec<&Expr>)> {
    let mut rights = Vec::new();
    let mut current = expression;
    while let Expr::JavaScriptBinary {
        left,
        op: JavaScriptBinaryOp::Add,
        right,
    } = current
    {
        rights.push(right.as_ref());
        current = left;
    }
    let Expr::String(first) = current else {
        return None;
    };
    if rights.is_empty() || rights.len() % 2 != 0 {
        return None;
    }
    rights.reverse();
    let mut quasis = vec![first.as_str()];
    let mut holes = Vec::with_capacity(rights.len() / 2);
    for pair in rights.chunks(2) {
        let [hole, Expr::String(quasi)] = pair else {
            return None;
        };
        holes.push(*hole);
        quasis.push(quasi.as_str());
    }
    Some((quasis, holes))
}

/// A template literal's raw text for `value`: the characters whose raw form
/// would read back differently (a backtick, a backslash, `${`, a carriage
/// return and the other controls) are escaped.
pub(super) fn template_text(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut characters = value.chars().peekable();
    while let Some(character) = characters.next() {
        match character {
            '`' => out.push_str("\\`"),
            '\\' => out.push_str("\\\\"),
            '$' if characters.peek() == Some(&'{') => out.push_str("\\$"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            control if control.is_control() => {
                out.push_str(&format!("\\u{{{:x}}}", u32::from(control)))
            }
            other => out.push(other),
        }
    }
    out
}
