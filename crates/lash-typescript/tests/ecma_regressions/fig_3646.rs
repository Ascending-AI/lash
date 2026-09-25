//! FIG-3646: the Test262 outcome record — fromCodePoint names its lone-surrogate refusal.

use super::*;

/// ECMA gives `String.fromCodePoint(0xD800)` a string of one lone code unit,
/// which the value model cannot hold: that is the registered lone-surrogate
/// refusal, not an invalid-argument error (FIG-3646, found by Test262's
/// `regExpUtils.js`).
#[test]
fn from_code_point_names_the_lone_surrogate_refusal() {
    for source in [
        "finish(String.fromCodePoint(0xD800));",
        "finish(String.fromCodePoint(0x41, 0xDFFF));",
    ] {
        let error = execute(source).expect_err("a lone surrogate is not representable");
        assert!(
            error.to_string().contains("TS_LONE_SURROGATE_UNSUPPORTED"),
            "{source}: {error}"
        );
    }
    let error = execute("finish(String.fromCodePoint(0x110000));")
        .expect_err("a code point past U+10FFFF is invalid");
    assert!(
        !error.to_string().contains("TS_LONE_SURROGATE_UNSUPPORTED"),
        "{error}"
    );
    assert_eq!(
        finished("finish(String.fromCodePoint(0x41, 0x1F600));"),
        Value::String("A\u{1F600}".into())
    );
}
