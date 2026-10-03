//! FIG-3720: an untagged template literal's value is its cooked text — each escape resolves per ECMA's `TemplateCharacter` TV — never its raw source text.

use super::*;

/// FIG-3720: an untagged template literal's value is its cooked text — each
/// escape resolves per ECMA's `TemplateCharacter` TV — never its raw source
/// text. `` `a\nb`.length `` is 3 in Node v25.2.1; raw text answered 4.
#[test]
fn untagged_template_literals_cook_their_escape_sequences() {
    for (source, expected) in [
        ("finish(`a\\nb`.length);", 3.0),
        ("finish(`\\t`.length);", 1.0),
        ("finish(`\\\\`.length);", 1.0),
        ("finish(`\\``.length);", 1.0),
        ("finish(`\\${x}`.length);", 4.0),
        ("finish(`\\A`.length);", 1.0),
        ("finish(`\\u{1F600}`.length);", 2.0),
        ("finish(`\\0`.length);", 1.0),
        // A line continuation contributes nothing.
        ("finish(`line\\\ncontinuation`.length);", 16.0),
        // A literal newline stays a newline; CR and CRLF both normalise to LF.
        ("finish(`a\nb`.length);", 3.0),
        ("finish(`x\ry`.length);", 3.0),
        ("finish(`x\r\ny`.length);", 3.0),
        // A code point past the BMP cooks to a surrogate pair.
        ("finish(`\\u{10FFFF}`.length);", 2.0),
    ] {
        assert_eq!(finished(source), Value::Number(expected), "{source}");
    }
    for (source, expected) in [
        ("finish(`\\x41`);", "A"),
        ("finish(`a\\n${1}\\tb`);", "a\n1\tb"),
        ("finish(`\\u{1F600}`);", "😀"),
        ("finish(`\\${x}`);", "${x}"),
        ("finish(`\\0`);", "\0"),
        // `\u` reads exactly four hex digits; the rest is ordinary text.
        ("finish(`\\u12345`);", "\u{1234}5"),
        // A code point escape's bound is on the value, not the digit count.
        ("finish(`\\u{000000041}`);", "A"),
    ] {
        assert_eq!(finished(source), Value::String(expected.into()), "{source}");
    }
}

/// The same escape list in ordinary string literals, under either quote.
/// These cooked correctly already; the rows pin the template fix against the
/// string path's answers.
#[test]
fn string_literals_cook_the_same_escapes_under_either_quote() {
    for (source, expected) in [
        ("finish('a\\nb'.length);", 3.0),
        ("finish(\"a\\nb\".length);", 3.0),
        ("finish('\\t'.length);", 1.0),
        ("finish(\"\\t\".length);", 1.0),
        ("finish('\\\\'.length);", 1.0),
        ("finish(\"\\\\\".length);", 1.0),
        ("finish('\\\''.length);", 1.0),
        ("finish(\"\\\"\".length);", 1.0),
        ("finish('\\${x}'.length);", 4.0),
        ("finish(\"\\${x}\".length);", 4.0),
        ("finish('\\A'.length);", 1.0),
        ("finish('\\u{1F600}'.length);", 2.0),
        ("finish(\"\\u{1F600}\".length);", 2.0),
        ("finish('\\0'.length);", 1.0),
        ("finish('line\\\ncontinuation'.length);", 16.0),
        ("finish(\"line\\\ncontinuation\".length);", 16.0),
    ] {
        assert_eq!(finished(source), Value::Number(expected), "{source}");
    }
    for (source, expected) in [
        ("finish('\\x41');", "A"),
        ("finish(\"\\x41\");", "A"),
        ("finish('a\\n${1}\\tb');", "a\n${1}\tb"),
        ("finish('\\u{1F600}');", "😀"),
    ] {
        assert_eq!(finished(source), Value::String(expected.into()), "{source}");
    }
}

/// An escape that cannot cook is a SyntaxError in an untagged template — ECMA
/// reserves the uninterpreted form for tags, which the dialect refuses — and
/// a cooked lone surrogate is unrepresentable like the string-literal form.
#[test]
fn untagged_templates_with_uncookable_escapes_are_early_errors() {
    for source in [
        "finish(`\\unicode`);",
        "finish(`\\x`);",
        "finish(`\\xg`);",
        "finish(`\\u{110000}`);",
        "finish(`\\u{}`);",
        "finish(`\\u{1F_639}`);",
        "finish(`\\u12`);",
        "finish(`\\01`);",
        "finish(`\\00`);",
        "finish(`\\8`);",
        "finish(`\\9`);",
    ] {
        let error =
            lash_typescript::validate(source).expect_err("an uncookable escape is a SyntaxError");
        assert_eq!(error.code.as_str(), "TS_SYNTAX_ERROR", "{source}: {error}");
    }
    for source in [r#"finish(`\uD800`);"#, r#"finish(`\u{D800}`);"#] {
        let error = lash_typescript::validate(source)
            .expect_err("a cooked lone surrogate is not representable");
        assert_eq!(
            error.code.as_str(),
            "TS_LONE_SURROGATE_LITERAL_UNSUPPORTED",
            "{source}"
        );
    }
}
