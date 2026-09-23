//! Test262 ingestion shared by the slice runner and the corpus laws: how a
//! vendored test becomes the one dialect program both of them execute or
//! round-trip. The vendored bytes stay upstream's; the bridging below is the
//! whole of what the runner changes.

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

fn supply_assertion_message(source: String, callee: &str) -> String {
    let mut output = String::with_capacity(source.len());
    let mut remaining = source.as_str();
    while let Some(start) = remaining.find(callee) {
        let arguments_start = start + callee.len();
        output.push_str(&remaining[..arguments_start]);
        let bytes = remaining.as_bytes();
        let mut stack = vec![b'('];
        let mut quote = None;
        let mut escaped = false;
        let mut commas = 0;
        let mut end = arguments_start;
        for (offset, byte) in bytes[arguments_start..].iter().copied().enumerate() {
            end = arguments_start + offset;
            if let Some(active_quote) = quote {
                if escaped {
                    escaped = false;
                } else if byte == b'\\' {
                    escaped = true;
                } else if byte == active_quote {
                    quote = None;
                }
                continue;
            }
            match byte {
                b'\'' | b'"' | b'`' => quote = Some(byte),
                b'(' | b'[' | b'{' => stack.push(byte),
                b',' if stack.len() == 1 => commas += 1,
                b')' => {
                    if stack.pop() == Some(b'(') && stack.is_empty() {
                        break;
                    }
                }
                b']' => {
                    assert_eq!(stack.pop(), Some(b'['), "balanced assertion argument");
                }
                b'}' => {
                    assert_eq!(stack.pop(), Some(b'{'), "balanced assertion argument");
                }
                _ => {}
            }
        }
        assert!(stack.is_empty(), "unterminated Test262 assertion call");
        output.push_str(&remaining[arguments_start..end]);
        if commas == 1 {
            output.push_str(", undefined");
        }
        remaining = &remaining[end..];
    }
    output.push_str(remaining);
    output
}

/// `assert.throws(ReferenceError, fn)` names the error class by its
/// constructor, which the dialect has no value for (constructors are not
/// first-class). The call is bridged to the shim with the class's name, which
/// the shim compares with the caught error's `name`.
fn name_expected_error_class(source: &str) -> String {
    const CALL: &str = "assert.throws(";
    let mut output = String::with_capacity(source.len());
    let mut remaining = source;
    while let Some(start) = remaining.find(CALL) {
        output.push_str(&remaining[..start]);
        let after = &remaining[start + CALL.len()..];
        let trimmed = after.trim_start();
        let class = trimmed
            .chars()
            .take_while(|character| character.is_ascii_alphanumeric() || *character == '_')
            .collect::<String>();
        let rest = trimmed[class.len()..].trim_start();
        assert!(
            !class.is_empty() && rest.starts_with(','),
            "assert.throws names its expected error class first"
        );
        output.push_str(&format!("assert[\"throws\"](\"{class}\""));
        remaining = rest;
    }
    output.push_str(remaining);
    output
}

pub(crate) fn source_for(path: &Path, test_metadata: &metadata::Metadata, finish: bool) -> String {
    let test = std::fs::read_to_string(path).expect("read vendored Test262 test");
    if test_metadata.flags.contains(&TestFlag::Raw) {
        return test;
    }

    // The dialect intentionally distinguishes known method calls from calls
    // through computed function-valued properties. The shim is a plain record,
    // not a production runtime global, so bridge only Test262's assertion
    // namespace to the latter spelling. Vendored tests remain byte-identical.
    let test = test
        .replace("new Test262Error(", "Test262Error(")
        .replace("assert.sameValue", "assert[\"sameValue\"]")
        .replace("assert.notSameValue", "assert[\"notSameValue\"]")
        .replace("assert.compareArray", "assert[\"compareArray\"]");
    let test = name_expected_error_class(&test);
    let test = [
        "assert[\"sameValue\"](",
        "assert[\"notSameValue\"](",
        "assert[\"compareArray\"](",
        "assert[\"throws\"](",
    ]
    .into_iter()
    .fold(test, supply_assertion_message);

    let mut source = String::new();
    for harness in ["sta.js", "assert.js", "compareArray.js"] {
        source.push_str(
            &std::fs::read_to_string(data_path(&format!("harness-shim/{harness}")))
                .expect("read Test262 harness shim"),
        );
        source.push('\n');
    }
    for include in test_metadata.includes.iter() {
        if include.as_ref() == "compareArray.js" {
            continue;
        }
        let include_path = data_path(&format!("harness-shim/{include}"));
        source.push_str(
            &std::fs::read_to_string(&include_path).unwrap_or_else(|error| {
                panic!(
                    "{} requires missing harness shim {}: {error}",
                    path.display(),
                    include_path.display()
                )
            }),
        );
        source.push('\n');
    }
    source.push_str(&test);
    if finish {
        source.push_str("\nfinish(true);\n");
    }
    source
}
