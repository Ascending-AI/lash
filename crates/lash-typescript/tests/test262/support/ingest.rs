//! Test262 ingestion shared by the conformance runner and the corpus laws: how
//! a vendored test becomes the one dialect program both of them execute or
//! round-trip. The vendored bytes stay upstream's; the bridging below is the
//! whole of what the runner changes, and it touches only code: a string,
//! template text, comment or regular-expression literal that happens to spell
//! `assert(` is left alone.

// FIG-2971: this file is test/tooling/host code; ambient fs/env/process
// access is sanctioned here (the workspace clippy ban targets production
// library code).
#![allow(clippy::disallowed_methods)]

use std::path::{Path, PathBuf};

use super::metadata::{self, TestFlag};

pub(crate) const ROOT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/test262");

pub(crate) fn data_path(relative: &str) -> PathBuf {
    Path::new(ROOT).join(relative)
}

/// The harness files every non-raw test runs after, in order: Test262's two
/// defined bindings (INTERPRETING.md). Upstream's `assert.js` also defines
/// `compareArray` and `assert.compareArray`, and so does its rendering.
const DEFAULT_HARNESS: [&str; 2] = ["sta.js", "assert.js"];

/// The file an `async` test runs after the default harness (INTERPRETING.md,
/// `flags: async`).
const ASYNC_HARNESS: &str = "doneprintHandle.js";

/// `source` with every character that is not code blanked to a space: string
/// and template text, comments and regular-expression literals. Offsets are
/// preserved, so a match found in the mask is a match at the same offset of
/// the source. Template substitutions are code and stay visible.
fn code_mask(source: &str) -> Vec<u8> {
    let bytes = source.as_bytes();
    let mut mask = bytes.to_vec();
    let mut index = 0;
    // Each entry is the brace depth at which an open template substitution
    // returns to template text.
    let mut templates: Vec<usize> = Vec::new();
    let mut depth = 0usize;
    // Whether a `/` here would start a regular expression rather than divide.
    let mut regex_allowed = true;
    let blank = |mask: &mut Vec<u8>, from: usize, to: usize| {
        for byte in &mut mask[from..to.min(bytes.len())] {
            if *byte != b'\n' {
                *byte = b' ';
            }
        }
    };
    // Template text from `start` (just after a backtick or a closing `}` of
    // a substitution) to its end; returns the index after it and whether it
    // opened a substitution.
    let template_text = |start: usize| -> (usize, bool) {
        let mut at = start;
        while at < bytes.len() {
            match bytes[at] {
                b'\\' => at += 2,
                b'`' => return (at + 1, false),
                b'$' if bytes.get(at + 1) == Some(&b'{') => return (at + 2, true),
                _ => at += 1,
            }
        }
        (bytes.len(), false)
    };
    while index < bytes.len() {
        let byte = bytes[index];
        match byte {
            b'/' if bytes.get(index + 1) == Some(&b'/') => {
                let end = source[index..]
                    .find('\n')
                    .map_or(bytes.len(), |end| index + end);
                blank(&mut mask, index, end);
                index = end;
            }
            b'/' if bytes.get(index + 1) == Some(&b'*') => {
                let end = source[index + 2..]
                    .find("*/")
                    .map_or(bytes.len(), |end| index + 2 + end + 2);
                blank(&mut mask, index, end);
                index = end;
            }
            b'/' if regex_allowed => {
                let mut at = index + 1;
                let mut class = false;
                while at < bytes.len() && bytes[at] != b'\n' {
                    match bytes[at] {
                        b'\\' => at += 1,
                        b'[' => class = true,
                        b']' => class = false,
                        b'/' if !class => break,
                        _ => {}
                    }
                    at += 1;
                }
                at += 1;
                while at < bytes.len() && (bytes[at].is_ascii_alphanumeric() || bytes[at] == b'_') {
                    at += 1;
                }
                blank(&mut mask, index, at);
                index = at;
                regex_allowed = false;
            }
            b'\'' | b'"' => {
                let mut at = index + 1;
                while at < bytes.len() && bytes[at] != byte && bytes[at] != b'\n' {
                    if bytes[at] == b'\\' {
                        at += 1;
                    }
                    at += 1;
                }
                blank(&mut mask, index, at + 1);
                index = at + 1;
                regex_allowed = false;
            }
            b'`' => {
                let (end, substitution) = template_text(index + 1);
                blank(&mut mask, index, if substitution { end - 2 } else { end });
                if substitution {
                    templates.push(depth);
                    depth += 1;
                }
                index = end;
                regex_allowed = substitution;
            }
            b'{' => {
                depth += 1;
                index += 1;
                regex_allowed = true;
            }
            b'}' => {
                depth = depth.saturating_sub(1);
                if templates.last() == Some(&depth) {
                    templates.pop();
                    let (end, substitution) = template_text(index + 1);
                    blank(
                        &mut mask,
                        index + 1,
                        if substitution { end - 2 } else { end },
                    );
                    if substitution {
                        templates.push(depth);
                        depth += 1;
                    }
                    index = end;
                    regex_allowed = substitution;
                } else {
                    index += 1;
                    // A block's closing brace starts a statement, where a
                    // regular expression can begin; an object literal's does
                    // not, but a division after one does not occur here.
                    regex_allowed = true;
                }
            }
            _ if byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'$' || byte >= 0x80 => {
                let start = index;
                while index < bytes.len()
                    && (bytes[index].is_ascii_alphanumeric()
                        || bytes[index] == b'_'
                        || bytes[index] == b'$'
                        || bytes[index] >= 0x80)
                {
                    index += 1;
                }
                regex_allowed = matches!(
                    &source[start..index],
                    "return"
                        | "typeof"
                        | "instanceof"
                        | "in"
                        | "of"
                        | "new"
                        | "delete"
                        | "void"
                        | "throw"
                        | "case"
                        | "do"
                        | "else"
                        | "yield"
                        | "await"
                );
            }
            b')' | b']' => {
                index += 1;
                regex_allowed = false;
            }
            _ if byte.is_ascii_whitespace() => index += 1,
            _ => {
                index += 1;
                regex_allowed = true;
            }
        }
    }
    mask
}

fn is_identifier_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'$'
}

/// Every offset where `word` occurs in the mask as a whole identifier that is
/// not a property name (not preceded by `.`).
fn identifier_offsets(mask: &[u8], word: &str) -> Vec<usize> {
    let word = word.as_bytes();
    let mut offsets = Vec::new();
    let mut at = 0;
    while at + word.len() <= mask.len() {
        if &mask[at..at + word.len()] == word
            && (at == 0 || (!is_identifier_byte(mask[at - 1]) && mask[at - 1] != b'.'))
            && mask
                .get(at + word.len())
                .is_none_or(|byte| !is_identifier_byte(*byte))
        {
            offsets.push(at);
            at += word.len();
        } else {
            at += 1;
        }
    }
    offsets
}

fn skip_spaces(mask: &[u8], mut at: usize) -> usize {
    while at < mask.len() && mask[at].is_ascii_whitespace() {
        at += 1;
    }
    at
}

fn identifier_at(mask: &[u8], at: usize) -> Option<(usize, usize)> {
    let end = (at..mask.len())
        .find(|index| !is_identifier_byte(mask[*index]))
        .unwrap_or(mask.len());
    (end > at && !mask[at].is_ascii_digit()).then_some((at, end))
}

/// One replacement of `source[start..end]`.
struct Edit {
    start: usize,
    end: usize,
    text: String,
}

fn apply(source: &str, mut edits: Vec<Edit>) -> String {
    edits.sort_by_key(|edit| edit.start);
    let mut output = String::with_capacity(source.len() + edits.len() * 8);
    let mut at = 0;
    for edit in edits {
        assert!(edit.start >= at, "overlapping Test262 ingestion edits");
        output.push_str(&source[at..edit.start]);
        output.push_str(&edit.text);
        at = edit.end;
    }
    output.push_str(&source[at..]);
    output
}

/// Bridges Test262's harness vocabulary to the dialect's spelling.
///
/// - `assert(...)`, a call of the `assert` function object, becomes
///   `__test262Assert(...)`: a dialect function cannot also carry the
///   `assert.*` methods, so the harness splits the two.
/// - `assert.name(...)` becomes `assert["name"](...)`: the dialect reserves
///   dotted method-call syntax for its method allowlist, and the harness's
///   `assert` is a plain record of functions.
/// - `assert.throws(C, ...)` names the expected class by its constructor,
///   which the dialect has no value for; the call passes the class's name,
///   and the harness compares it with the caught error's `name`.
/// - `new Test262Error(...)` constructs through the callable harness factory,
///   and `Test262Error.thrower` is the factory's thrower.
fn bridge(source: &str) -> String {
    let mask = code_mask(source);
    let mut edits = Vec::new();
    for at in identifier_offsets(&mask, "assert") {
        let after = skip_spaces(&mask, at + "assert".len());
        match mask.get(after) {
            Some(b'(') => edits.push(Edit {
                start: at,
                end: at + "assert".len(),
                text: "__test262Assert".to_owned(),
            }),
            Some(b'.') => {
                let Some((name_start, name_end)) =
                    identifier_at(&mask, skip_spaces(&mask, after + 1))
                else {
                    continue;
                };
                let name = &source[name_start..name_end];
                edits.push(Edit {
                    start: after,
                    end: name_end,
                    text: format!("[\"{name}\"]"),
                });
                let open = skip_spaces(&mask, name_end);
                if name == "throws" && mask.get(open) == Some(&b'(') {
                    let class_start = skip_spaces(&mask, open + 1);
                    if let Some((class_start, class_end)) = identifier_at(&mask, class_start)
                        && mask.get(skip_spaces(&mask, class_end)) == Some(&b',')
                    {
                        edits.push(Edit {
                            start: class_start,
                            end: class_end,
                            text: format!("\"{}\"", &source[class_start..class_end]),
                        });
                    }
                }
            }
            _ => {}
        }
    }
    for at in identifier_offsets(&mask, "new") {
        let class = skip_spaces(&mask, at + "new".len());
        if identifier_at(&mask, class)
            .is_some_and(|(start, end)| &source[start..end] == "Test262Error")
        {
            edits.push(Edit {
                start: at,
                end: class,
                text: String::new(),
            });
        }
    }
    for at in identifier_offsets(&mask, "Test262Error") {
        let dot = skip_spaces(&mask, at + "Test262Error".len());
        if mask.get(dot) == Some(&b'.')
            && let Some((start, end)) = identifier_at(&mask, skip_spaces(&mask, dot + 1))
            && &source[start..end] == "thrower"
        {
            edits.push(Edit {
                start: at,
                end,
                text: "__test262ErrorThrower".to_owned(),
            });
        }
    }
    apply(source, edits)
}

/// The harness file a test runs with, or `None` when the dialect has no shim
/// for it.
pub(crate) fn harness_shim(include: &str) -> Option<String> {
    std::fs::read_to_string(data_path(&format!("harness-shim/{include}"))).ok()
}

/// The program a test runs as: the harness (Test262's defined bindings, the
/// async print handle for an `async` test, then each include), then the test
/// bridged to the dialect, then, when `finish` is set, `finish(true)`.
///
/// # Panics
///
/// Panics when an include has no shim; the runner classifies that first.
pub(crate) fn source_for(path: &Path, test_metadata: &metadata::Metadata, finish: bool) -> String {
    assemble(path, test_metadata, finish, false)
}

/// [`source_for`] without the includes that have no shim: the program the
/// test's own body is, as far as the dialect can build it, to learn whether
/// that body refuses before any missing include matters.
pub(crate) fn source_without_unshimmed(
    path: &Path,
    test_metadata: &metadata::Metadata,
    finish: bool,
) -> String {
    assemble(path, test_metadata, finish, true)
}

/// The harness files a test runs after, in evaluation order.
fn harness_files(test_metadata: &metadata::Metadata) -> Vec<&str> {
    if test_metadata.flags.contains(&TestFlag::Raw) {
        return Vec::new();
    }
    let mut harness = DEFAULT_HARNESS.to_vec();
    if test_metadata.flags.contains(&TestFlag::Async) {
        harness.push(ASYNC_HARNESS);
    }
    for include in test_metadata.includes.iter() {
        if !harness.contains(&include.as_ref()) {
            harness.push(include);
        }
    }
    harness
}

/// The test alone, bridged, then `finish(true)` when `finish` is set: the
/// Script that runs after the harness.
pub(crate) fn test_script(path: &Path, test_metadata: &metadata::Metadata, finish: bool) -> String {
    let test = std::fs::read_to_string(path).expect("read vendored Test262 test");
    if test_metadata.flags.contains(&TestFlag::Raw) {
        return test;
    }
    let mut script = bridge(&test);
    if finish {
        script.push_str("\nfinish(true);\n");
    }
    script
}

/// The global names the test's harness binds: every top-level declaration of
/// its rendered files, which the test links against as a later Script would.
pub(crate) fn harness_bindings(
    test_metadata: &metadata::Metadata,
) -> std::collections::BTreeSet<String> {
    let mut names = std::collections::BTreeSet::new();
    for include in harness_files(test_metadata) {
        let Some(shim) = harness_shim(include) else {
            continue;
        };
        for line in shim.lines() {
            let declared = ["function ", "async function ", "const ", "let ", "var "]
                .iter()
                .find_map(|keyword| line.strip_prefix(keyword));
            if let Some(rest) = declared {
                let name = rest
                    .split(|character: char| {
                        !(character.is_ascii_alphanumeric() || character == '_' || character == '$')
                    })
                    .next()
                    .unwrap_or_default();
                if !name.is_empty() {
                    names.insert(name.to_owned());
                }
            }
        }
    }
    names
}

fn assemble(
    path: &Path,
    test_metadata: &metadata::Metadata,
    finish: bool,
    omit_unshimmed: bool,
) -> String {
    if test_metadata.flags.contains(&TestFlag::Raw) {
        return std::fs::read_to_string(path).expect("read vendored Test262 test");
    }
    let mut source = String::new();
    for include in harness_files(test_metadata) {
        match harness_shim(include) {
            Some(shim) => source.push_str(&shim),
            None if omit_unshimmed => continue,
            None => panic!(
                "{} requires harness file {include}, which has no shim",
                path.display()
            ),
        }
        source.push('\n');
    }
    source.push_str(&test_script(path, test_metadata, finish));
    source
}

#[cfg(test)]
mod tests {
    use super::bridge;

    #[test]
    fn bridging_touches_code_only() {
        let source = concat!(
            "assert(x, 'assert(y)'); assert.sameValue(a, b); // assert(z)\n",
            "assert.throws(TypeError, f); /assert(/.test(s); `assert(${assert(q)})`;\n",
            "throw new Test262Error('new Test262Error(');\n",
            "o.assert(1); Test262Error.thrower('m');\n",
        );
        assert_eq!(
            bridge(source),
            concat!(
                "__test262Assert(x, 'assert(y)'); assert[\"sameValue\"](a, b); // assert(z)\n",
                "assert[\"throws\"](\"TypeError\", f); /assert(/.test(s); `assert(${__test262Assert(q)})`;\n",
                "throw Test262Error('new Test262Error(');\n",
                "o.assert(1); __test262ErrorThrower('m');\n",
            )
        );
    }
}
