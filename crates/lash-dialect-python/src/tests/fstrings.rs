//! PY-FSTRING: an f-string concatenates its parts in order, with no list
//! and no general join (FIG-6846). Python formats every field before it
//! builds the text, so with more than one field each is bound where it is
//! formatted, before any concatenation.

use super::machine::{kernel_text, lower, run};

/// Whether an action runs inside a native call's operands in `node`.
fn action_in_call(node: lash_kernel_doc::Node<'_>, in_call: bool) -> bool {
    use lash_kernel_doc::{Expr, Node};

    if in_call && matches!(node, Node::Expr(Expr::Act(_))) {
        return true;
    }
    let in_call = in_call || matches!(node, Node::Expr(Expr::Call { .. }));
    node.children()
        .into_iter()
        .any(|child| action_in_call(child, in_call))
}

/// A key built by an f-string is the key built by `+`: one concatenation
/// of the literal and the field's text, with no list and no join.
#[test]
fn an_fstring_key_lowers_to_one_concatenation() {
    let template = kernel_text("def f(n):\n    return f\"key_{n % 64}\"\n");
    let plus = kernel_text("def f(n):\n    return \"key_\" + str(n % 64)\n");
    assert!(template.contains("text.concat(\"key_\", "), "{template}");
    for general in ["text.join(", "list["] {
        assert!(!template.contains(general), "keeps {general}:\n{template}");
    }
    assert_eq!(
        template.matches("text.concat(").count(),
        plus.matches("text.concat(").count(),
        "{template}\n{plus}"
    );
}

/// With more than one field, every field is formatted, and bound, before
/// the first concatenation: no formatting action runs inside the
/// concatenations' operands.
#[test]
fn fields_are_formatted_before_the_text_is_built() {
    let source = "def f(a, b, c):\n    return f\"<{a}|{b!r:>6}|{c:.2f}>\"\n";
    let lowered = lower(source).unwrap_or_else(|error| panic!("{error}"));
    let text = kernel_text(source);
    assert!(text.matches("text.concat(").count() >= 6, "{text}");
    assert!(!text.contains("text.join("), "{text}");
    for function in lowered.document.functions.values().filter(|function| {
        matches!(
            function.implementation,
            lash_kernel_doc::Implementation::Guest {
                metering: lash_kernel_doc::Metering::Statements,
                ..
            }
        )
    }) {
        assert!(
            !action_in_call(
                lash_kernel_doc::Node::Block(&function.body().unwrap().block),
                false
            ),
            "a field formats inside a concatenation:\n{text}"
        );
    }
}

/// Literals, escaped braces, implicit concatenation, conversions and
/// format specifications give the text CPython gives.
#[test]
fn fstrings_give_the_text_python_gives() {
    let source = r#"n = 7
name = "ab"
x = 2.5
long = "a-long-text-past-the-inline-limit"
log = []
def tick(tag):
    log.append(tag)
    return tag
print(f"key_{n % 64}")
print(f"{n:>4}|{name!r}|{x:.2f}|{n:x}|{name!s}")
print("pre" f"-{n}-" "post")
print(f"{{}}{n}{{")
print(f"" + "|")
print(f"plain")
print(f"{name}")
print(f"{long}{n}{name}")
print(f"{tick('a')}{tick('b'):>3}{tick('c')!r}", log)
"#;
    let ran = run(source, &[]);
    assert_eq!(ran.end, "ok", "{}", kernel_text(source));
    assert_eq!(
        ran.lines(),
        [
            "key_7",
            "   7|'ab'|2.50|7|ab",
            "pre-7-post",
            "{}7{",
            "|",
            "plain",
            "ab",
            "a-long-text-past-the-inline-limit7ab",
            "a  b'c' ['a', 'b', 'c']",
        ]
    );
}
