// We like hashes around raw string literals.
#![allow(clippy::needless_raw_string_hashes)]

// Work around dead code warnings: rust-lang issue #46379
#[allow(dead_code)]
#[path = "common/mod.rs"]
pub mod common;
use common::*;

#[test]
fn run_parse_should_fail_tests() {
    test_parse_fails(r#"x{5,4}"#);
    test_parse_fails(r#"[abcd"#);
    test_parse_fails(r#"[z-a]"#);
    test_parse_fails(r#"^*"#);
    test_parse_fails(r#"(abc"#);
    test_parse_fails(r#"(?# abc"#);
    test_parse_fails(r#"{4,5}abc"#);
    test_parse_fails(r#")"#);
    test_parse_fails(r#"a[b-a]"#);
    test_parse_fails(r#"a["#);
    test_parse_fails(r#"*a"#);
    test_parse_fails(r#"abc)"#);
    test_parse_fails(r#"(abc"#);
    test_parse_fails(r#"a**"#);
    test_parse_fails(r#")("#);
    test_parse_fails(r#"a[b-a]"#);
    test_parse_fails(r#"a["#);
    test_parse_fails(r#"*a"#);
    test_parse_fails(r#"abc)"#);
    test_parse_fails(r#"(abc"#);
    test_parse_fails(r#"a**"#);
    test_parse_fails(r#")("#);
    test_parse_fails(r#":(?:"#);
    test_parse_fails(r#"a(?{)b"#);
    test_parse_fails(r#"a(?{{})b"#);
    test_parse_fails(r#"a(?{}})b"#);
    test_parse_fails(r#"a(?{"{"})b"#);
    test_parse_fails(r#"a(?{"{"}})b"#);
    test_parse_fails(r#"[a[:xyz:"#);
    test_parse_fails(r#"a{37,17}"#);
    test_parse_fails(r#"[\200-\110]"#);
    test_parse_fails(r#"["#);
    test_parse_fails(r#"[a-"#);
    test_parse_fails(r#"^[a-\Q\E]"#);
    test_parse_fails(r#"(ab|c)(?-1)"#);
    test_parse_fails(r#"x(?-0)y"#);
    test_parse_fails(r#"x(?-1)y"#);
    test_parse_fails(r#"(?|(abc)|(xyz))"#);
    test_parse_fails(r#"(x)(?|(abc)|(xyz))(x)"#);
    test_parse_fails(r#"(x)(?|(abc)(pqr)|(xyz))(x)"#);
    test_parse_fails(r#"(?|(abc)|(xyz))\1"#);
    test_parse_fails(r#"(?-+a)"#);
    test_parse_fails(r#"^\ca\cA\c[\c{\c:"#);
    test_parse_fails(r#"a(?)b"#);
    test_parse_fails(r#"(?|(abc)|(xyz))"#);
    test_parse_fails(r#"(x)(?|(abc)|(xyz))(x)"#);
    test_parse_fails(r#"(x)(?|(abc)(pqr)|(xyz))(x)"#);
}

#[test]
fn run_pcre_match_tests() {
    test_with_configs(run_pcre_match_tests_config)
}

#[rustfmt::skip]
fn run_pcre_match_tests_config(tc: TestConfig) {
    let run1_match = |pattern: &str, flags_str: &str, input: &str| -> String {
        let cr = tc.compilef(pattern, flags_str);
        cr.match1f(input)
    };

    let test_eq = |left: String, right: &str| {
        assert_eq!(left.as_str(), right)
    };

    test_eq(run1_match("abc", "i", "abc"), "abc");
    test_eq(run1_match("abc", "i", "defabc"), "abc");
    test_eq(run1_match("abc", "i", "Aabc"), "abc");
    test_eq(run1_match("abc", "i", "Adefabc"), "abc");
    test_eq(run1_match("abc", "i", "ABC"), "ABC");
    test_eq(run1_match("^abc", "i", "abc"), "abc");
    test_eq(run1_match("^abc$", "i", "abc"), "abc");
    test_eq(run1_match("cat|dog|elephant", "i", "this sentence eventually mentions a cat"), "cat"); // "cat|dog|elephant"
    test_eq(run1_match("cat|dog|elephant", "i", "this sentences rambles on and on for a while and then reaches elephant"), "elephant"); // "cat|dog|elephant"
    test_eq(run1_match("cat|dog|elephant", "i", "this sentence eventually mentions a cat"), "cat"); // "cat|dog|elephant"
    test_eq(run1_match("cat|dog|elephant", "i", "this sentences rambles on and on for a while and then reaches elephant"), "elephant"); // "cat|dog|elephant"
    test_eq(run1_match("cat|dog|elephant", "i", "this sentence eventually mentions a CAT cat"), "CAT"); // "cat|dog|elephant"
    test_eq(run1_match("cat|dog|elephant", "i", "this sentences rambles on and on for a while to elephant ElePhant"), "elephant"); // "cat|dog|elephant"
    test_eq(run1_match("(a)(b)(c)\\2", "i", "abcb"), "abcb,a,b,c");
    test_eq(run1_match("(a)(b)(c)\\2", "i", "O0abcb"), "abcb,a,b,c");
    test_eq(run1_match("(a)(b)(c)\\2", "i", "O3abcb"), "abcb,a,b,c");
    test_eq(run1_match("(a)(b)(c)\\2", "i", "O6abcb"), "abcb,a,b,c");
    test_eq(run1_match("(a)(b)(c)\\2", "i", "O9abcb"), "abcb,a,b,c");
    test_eq(run1_match("(a)(b)(c)\\2", "i", "O12abcb"), "abcb,a,b,c");
    test_eq(run1_match("(a)bc|(a)(b)\\2", "i", "abc"), "abc,a,,");
    test_eq(run1_match("(a)bc|(a)(b)\\2", "i", "O0abc"), "abc,a,,");
    test_eq(run1_match("(a)bc|(a)(b)\\2", "i", "O3abc"), "abc,a,,");
    test_eq(run1_match("(a)bc|(a)(b)\\2", "i", "O6abc"), "abc,a,,");
    test_eq(run1_match("(a)bc|(a)(b)\\2", "i", "aba"), "aba,,a,b");
    test_eq(run1_match("(a)bc|(a)(b)\\2", "i", "O0aba"), "aba,,a,b");
    test_eq(run1_match("(a)bc|(a)(b)\\2", "i", "O3aba"), "aba,,a,b");
    test_eq(run1_match("(a)bc|(a)(b)\\2", "i", "O6aba"), "aba,,a,b");
    test_eq(run1_match("(a)bc|(a)(b)\\2", "i", "O9aba"), "aba,,a,b");
    test_eq(run1_match("(a)bc|(a)(b)\\2", "i", "O12aba"), "aba,,a,b");
    test_eq(run1_match("abc$", "i", "abc"), "abc");
    test_eq(run1_match("the quick brown fox", "i", "the quick brown fox"), "the quick brown fox"); // "the quick brown fox"
    test_eq(run1_match("the quick brown fox", "i", "this is a line with the quick brown fox"), "the quick brown fox"); // "the quick brown fox"
    test_eq(run1_match("^abc|def", "i", "abcdef"), "abc"); // "^abc|def"
    test_eq(run1_match("^abc|def", "i", "abcdefB"), "abc"); // "^abc|def"
    test_eq(run1_match(".*((abc)$|(def))", "i", "defabc"), "defabc,abc,abc,"); // ".*((abc)$|(def))"
    test_eq(run1_match(".*((abc)$|(def))", "i", "Zdefabc"), "Zdefabc,abc,abc,"); // ".*((abc)$|(def))"
    test_eq(run1_match("abc", "i", "abc"), "abc");
    test_eq(run1_match("^abc|def", "i", "abcdef"), "abc"); // "^abc|def"
    test_eq(run1_match("^abc|def", "i", "abcdefB"), "abc"); // "^abc|def"
    test_eq(run1_match(".*((abc)$|(def))", "i", "defabc"), "defabc,abc,abc,"); // ".*((abc)$|(def))"
    test_eq(run1_match(".*((abc)$|(def))", "i", "Zdefabc"), "Zdefabc,abc,abc,"); // ".*((abc)$|(def))"
    test_eq(run1_match("the quick brown fox", "i", "the quick brown fox"), "the quick brown fox"); // "the quick brown fox"
    test_eq(run1_match("the quick brown fox", "i", "The Quick Brown Fox"), "The Quick Brown Fox"); // "the quick brown fox"
    test_eq(run1_match("the quick brown fox", "i", "the quick brown fox"), "the quick brown fox"); // "the quick brown fox"
    test_eq(run1_match("the quick brown fox", "i", "The Quick Brown Fox"), "The Quick Brown Fox"); // "the quick brown fox"
    test_eq(run1_match("abc$", "i", "abc"), "abc");
    test_eq(run1_match("(abc\\1)", "i", "abc"), "abc,abc");
    test_eq(run1_match("[^aeiou ]{3,}", "i", "co-processors, and for"), "-pr");
    test_eq(run1_match("<.*>", "i", "abc<def>ghi<klm>nop"), "<def>ghi<klm>");
    test_eq(run1_match("<.*?>", "i", "abc<def>ghi<klm>nop"), "<def>");
    test_eq(run1_match("<.*?>", "i", "abc<def>ghi<klm>nop"), "<def>");
    test_eq(run1_match("a$", "i", "a"), "a");
    test_eq(run1_match("a$", "i", "Za"), "a");
    test_eq(run1_match("a$", "im", "a"), "a");
    test_eq(run1_match("a$", "im", "a\n"), "a");
    test_eq(run1_match("a$", "im", "Za\n"), "a");
    test_eq(run1_match("a$", "im", "Za"), "a");
    test_eq(run1_match("(?!alphabet)[ab]", "i", "foo\nbarbar"), "b");
    test_eq(run1_match("(?!alphabet)[ab]", "i", "***Failers"), "a");
    test_eq(run1_match("(?!alphabet)[ab]", "i", "rhubarb"), "b");
    test_eq(run1_match("(?!alphabet)[ab]", "i", "barbell"), "b");
    test_eq(run1_match("(?!alphabet)[ab]", "i", "abc\nbarton"), "a");
    test_eq(run1_match("(?!alphabet)[ab]", "i", "foo\nbarbar"), "b");
    test_eq(run1_match("(?!alphabet)[ab]", "i", "***Failers"), "a");
    test_eq(run1_match("(?!alphabet)[ab]", "i", "rhubarb"), "b");
    test_eq(run1_match("(?!alphabet)[ab]", "i", "barbell"), "b");
    test_eq(run1_match("(?!alphabet)[ab]", "i", "abc\nbarton"), "a");
    test_eq(run1_match("(?!alphabet)[ab]", "i", "abc"), "a");
    test_eq(run1_match("(?!alphabet)[ab]", "i", "def\nabc"), "a");
    test_eq(run1_match("(?!alphabet)[ab]", "i", "*** Failers"), "a");
    test_eq(run1_match("(?!alphabet)[ab]", "i", "defabc"), "a");
    test_eq(run1_match("(a)bc(d)", "i", "abcd"), "abcd,a,d");
    test_eq(run1_match("(a)bc(d)", "i", "abcdC2"), "abcd,a,d");
    test_eq(run1_match("(a)bc(d)", "i", "abcdC5"), "abcd,a,d");
    test_eq(run1_match("(.{20})", "i", "abcdefghijklmnopqrstuvwxyz"), "abcdefghijklmnopqrst,abcdefghijklmnopqrst");
    test_eq(run1_match("(.{20})", "i", "abcdefghijklmnopqrstuvwxyzC1"), "abcdefghijklmnopqrst,abcdefghijklmnopqrst");
    test_eq(run1_match("(.{20})", "i", "abcdefghijklmnopqrstuvwxyzG1"), "abcdefghijklmnopqrst,abcdefghijklmnopqrst");
    test_eq(run1_match("(.{15})", "i", "abcdefghijklmnopqrstuvwxyz"), "abcdefghijklmno,abcdefghijklmno");
    test_eq(run1_match("(.{15})", "i", "abcdefghijklmnopqrstuvwxyzC1G1"), "abcdefghijklmno,abcdefghijklmno");
    test_eq(run1_match("(.{16})", "i", "abcdefghijklmnopqrstuvwxyz"), "abcdefghijklmnop,abcdefghijklmnop");
    test_eq(run1_match("(.{16})", "i", "abcdefghijklmnopqrstuvwxyzC1G1L"), "abcdefghijklmnop,abcdefghijklmnop");
    test_eq(run1_match("^(a|(bc))de(f)", "i", "adefG1G2G3G4L"), "adef,a,,f");
    test_eq(run1_match("^(a|(bc))de(f)", "i", "bcdefG1G2G3G4L"), "bcdef,bc,bc,f");
    test_eq(run1_match("^(a|(bc))de(f)", "i", "adefghijkC0"), "adef,a,,f"); // "^(a|(bc))de(f)"
    // Skipping Unicode-unfriendly ^abc\00def
    test_eq(run1_match("\\Biss\\B", "i", "Mississippi"), "iss");
    test_eq(tc.compilef("iss", "i").run_global_match("Mississippi"), "iss,iss");
    test_eq(tc.compilef("\\Biss\\B", "i").run_global_match("Mississippi"), "iss,iss"); // "\\Biss\\B"
    // Skipping global "\\Biss\\B" with string "MississippiA"
    // Skipping global "\\Biss\\B" with string "Mississippi"
    test_eq(tc.compilef("^iss", "i").run_global_match("ississippi"), "iss");
    test_eq(tc.compilef(".*iss", "i").run_global_match("abciss\nxyzisspqr"), "abciss,xyziss");
    test_eq(tc.compilef(".i.", "i").run_global_match("Mississippi"), "Mis,sis,sip"); // ".i."
    // Skipping Unicode-unfriendly .i.
    // Skipping Unicode-unfriendly .i.
    // Skipping Unicode-unfriendly .i.
    test_eq(tc.compilef("^.is", "i").run_global_match("Mississippi"), "Mis");
    test_eq(tc.compilef("^ab\\n", "i").run_global_match("ab\nab\ncd"), "ab\n");
    test_eq(tc.compilef("^ab\\n", "im").run_global_match("ab\nab\ncd"), "ab\n,ab\n");
    test_eq(run1_match("a?b?", "i", "a"), "a");
    test_eq(run1_match("a?b?", "i", "b"), "b");
    test_eq(run1_match("a?b?", "i", "ab"), "ab");
    test_eq(run1_match("a?b?", "i", "\\"), "");
    test_eq(run1_match("a?b?", "i", "*** Failers"), "");
    test_eq(run1_match("a?b?", "i", "N"), "");
    test_eq(run1_match("|-", "i", "abcd"), "");
    test_eq(run1_match("|-", "i", "-abc"), "");
    test_eq(run1_match("|-", "i", "Nab-c"), "");
    test_eq(run1_match("|-", "i", "*** Failers"), "");
    test_eq(run1_match("|-", "i", "Nabc"), "");
    test_eq(run1_match("a*(b+)(z)(z)", "i", "aaaabbbbzzzz"), "aaaabbbbzz,bbbb,z,z");
    test_eq(run1_match("a*(b+)(z)(z)", "i", "aaaabbbbzzzzO0"), "aaaabbbbzz,bbbb,z,z");
    test_eq(run1_match("a*(b+)(z)(z)", "i", "aaaabbbbzzzzO1"), "aaaabbbbzz,bbbb,z,z");
    test_eq(run1_match("a*(b+)(z)(z)", "i", "aaaabbbbzzzzO2"), "aaaabbbbzz,bbbb,z,z");
    test_eq(run1_match("a*(b+)(z)(z)", "i", "aaaabbbbzzzzO3"), "aaaabbbbzz,bbbb,z,z");
    test_eq(run1_match("a*(b+)(z)(z)", "i", "aaaabbbbzzzzO4"), "aaaabbbbzz,bbbb,z,z");
    test_eq(run1_match("a*(b+)(z)(z)", "i", "aaaabbbbzzzzO5"), "aaaabbbbzz,bbbb,z,z");
    test_eq(run1_match("^.?abcd", "i", "(abcd)"), "(abcd");
    test_eq(run1_match("^.?abcd", "i", "(abcd)xyz"), "(abcd");
    test_eq(run1_match("^.?abcd", "i", "abcd"), "abcd");
    test_eq(run1_match("^.?abcd", "i", "abcd)"), "abcd");
    test_eq(run1_match("^.?abcd", "i", "(abcd"), "(abcd");
    test_eq(run1_match("^.?abcd", "i", "(abcd)"), "(abcd");
    test_eq(run1_match("^.?abcd", "i", "(abcd(xyz<p>qrs)123)"), "(abcd");
    test_eq(run1_match("(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\d+(?:\\s|$))(\\w+)\\s+(\\270)", "i", "O900 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 19 20 21 22 23 24 25 26 27 28 29 30 31 32 33 34 35 36 37 38 39 40 41 42 43 44 45 46 47 48 49 50 51 52 53 54 55 56 57 58 59 60 61 62 63 64 65 66 67 68 69 70 71 72 73 74 75 76 77 78 79 80 81 82 83 84 85 86 87 88 89 90 91 92 93 94 95 96 97 98 99 100 101 102 103 104 105 106 107 108 109 110 111 112 113 114 115 116 117 118 119 120 121 122 123 124 125 126 127 128 129 130 131 132 133 134 135 136 137 138 139 140 141 142 143 144 145 146 147 148 149 150 151 152 153 154 155 156 157 158 159 160 161 162 163 164 165 166 167 168 169 170 171 172 173 174 175 176 177 178 179 180 181 182 183 184 185 186 187 188 189 190 191 192 193 194 195 196 197 198 199 200 201 202 203 204 205 206 207 208 209 210 211 212 213 214 215 216 217 218 219 220 221 222 223 224 225 226 227 228 229 230 231 232 233 234 235 236 237 238 239 240 241 242 243 244 245 246 247 248 249 250 251 252 253 254 255 256 257 258 259 260 261 262 263 264 265 266 267 268 269 ABC ABC"), "1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 19 20 21 22 23 24 25 26 27 28 29 30 31 32 33 34 35 36 37 38 39 40 41 42 43 44 45 46 47 48 49 50 51 52 53 54 55 56 57 58 59 60 61 62 63 64 65 66 67 68 69 70 71 72 73 74 75 76 77 78 79 80 81 82 83 84 85 86 87 88 89 90 91 92 93 94 95 96 97 98 99 100 101 102 103 104 105 106 107 108 109 110 111 112 113 114 115 116 117 118 119 120 121 122 123 124 125 126 127 128 129 130 131 132 133 134 135 136 137 138 139 140 141 142 143 144 145 146 147 148 149 150 151 152 153 154 155 156 157 158 159 160 161 162 163 164 165 166 167 168 169 170 171 172 173 174 175 176 177 178 179 180 181 182 183 184 185 186 187 188 189 190 191 192 193 194 195 196 197 198 199 200 201 202 203 204 205 206 207 208 209 210 211 212 213 214 215 216 217 218 219 220 221 222 223 224 225 226 227 228 229 230 231 232 233 234 235 236 237 238 239 240 241 242 243 244 245 246 247 248 249 250 251 252 253 254 255 256 257 258 259 260 261 262 263 264 265 266 267 268 269 ABC ABC,1 ,2 ,3 ,4 ,5 ,6 ,7 ,8 ,9 ,10 ,11 ,12 ,13 ,14 ,15 ,16 ,17 ,18 ,19 ,20 ,21 ,22 ,23 ,24 ,25 ,26 ,27 ,28 ,29 ,30 ,31 ,32 ,33 ,34 ,35 ,36 ,37 ,38 ,39 ,40 ,41 ,42 ,43 ,44 ,45 ,46 ,47 ,48 ,49 ,50 ,51 ,52 ,53 ,54 ,55 ,56 ,57 ,58 ,59 ,60 ,61 ,62 ,63 ,64 ,65 ,66 ,67 ,68 ,69 ,70 ,71 ,72 ,73 ,74 ,75 ,76 ,77 ,78 ,79 ,80 ,81 ,82 ,83 ,84 ,85 ,86 ,87 ,88 ,89 ,90 ,91 ,92 ,93 ,94 ,95 ,96 ,97 ,98 ,99 ,100 ,101 ,102 ,103 ,104 ,105 ,106 ,107 ,108 ,109 ,110 ,111 ,112 ,113 ,114 ,115 ,116 ,117 ,118 ,119 ,120 ,121 ,122 ,123 ,124 ,125 ,126 ,127 ,128 ,129 ,130 ,131 ,132 ,133 ,134 ,135 ,136 ,137 ,138 ,139 ,140 ,141 ,142 ,143 ,144 ,145 ,146 ,147 ,148 ,149 ,150 ,151 ,152 ,153 ,154 ,155 ,156 ,157 ,158 ,159 ,160 ,161 ,162 ,163 ,164 ,165 ,166 ,167 ,168 ,169 ,170 ,171 ,172 ,173 ,174 ,175 ,176 ,177 ,178 ,179 ,180 ,181 ,182 ,183 ,184 ,185 ,186 ,187 ,188 ,189 ,190 ,191 ,192 ,193 ,194 ,195 ,196 ,197 ,198 ,199 ,200 ,201 ,202 ,203 ,204 ,205 ,206 ,207 ,208 ,209 ,210 ,211 ,212 ,213 ,214 ,215 ,216 ,217 ,218 ,219 ,220 ,221 ,222 ,223 ,224 ,225 ,226 ,227 ,228 ,229 ,230 ,231 ,232 ,233 ,234 ,235 ,236 ,237 ,238 ,239 ,240 ,241 ,242 ,243 ,244 ,245 ,246 ,247 ,248 ,249 ,250 ,251 ,252 ,253 ,254 ,255 ,256 ,257 ,258 ,259 ,260 ,261 ,262 ,263 ,264 ,265 ,266 ,267 ,268 ,269 ,ABC,ABC");
    test_eq(run1_match("(main(O)?)+", "i", "mainmain"), "mainmain,main,");
    test_eq(run1_match("(main(O)?)+", "i", "mainOmain"), "mainOmain,main,");
    test_eq(run1_match("^(a(b)?)+$", "i", "aba"), "aba,a,");
    test_eq(run1_match("^(aa(bb)?)+$", "i", "aabbaa"), "aabbaa,aa,");
    test_eq(run1_match("^(aa|aa(bb))+$", "i", "aabbaa"), "aabbaa,aa,");
    test_eq(run1_match("^(aa(bb)??)+$", "i", "aabbaa"), "aabbaa,aa,");
    test_eq(run1_match("^(?:aa(bb)?)+$", "i", "aabbaa"), "aabbaa,");
    test_eq(run1_match("^(aa(b(b))?)+$", "i", "aabbaa"), "aabbaa,aa,,");
    test_eq(run1_match("^(?:aa(b(b))?)+$", "i", "aabbaa"), "aabbaa,,");
    test_eq(run1_match("^(?:aa(b(?:b))?)+$", "i", "aabbaa"), "aabbaa,");
    test_eq(run1_match("^(?:aa(bb(?:b))?)+$", "i", "aabbbaa"), "aabbbaa,");
    test_eq(run1_match("^(?:aa(b(?:bb))?)+$", "i", "aabbbaa"), "aabbbaa,");
    test_eq(run1_match("^(?:aa(?:b(b))?)+$", "i", "aabbaa"), "aabbaa,");
    test_eq(run1_match("^(?:aa(?:b(bb))?)+$", "i", "aabbbaa"), "aabbbaa,");
    test_eq(run1_match("^(aa(b(bb))?)+$", "i", "aabbbaa"), "aabbbaa,aa,,");
    test_eq(run1_match("^(aa(bb(bb))?)+$", "i", "aabbbbaa"), "aabbbbaa,aa,,");
    test_eq(run1_match("[\\S]", "", "ab"), "a");
    test_eq(run1_match("[\\S]", "", "aB"), "a");
    test_eq(run1_match("[\\S]", "", "*** Failers"), "*");
    test_eq(run1_match("[\\S]", "", "AB"), "A");
    test_eq(run1_match("[\\S]", "", "ab"), "a");
    test_eq(run1_match("[\\S]", "", "aB"), "a");
    test_eq(run1_match("[\\S]", "", "*** Failers"), "*");
    test_eq(run1_match("[\\S]", "", "AB"), "A");
    test_eq(run1_match("((.*))\\d+\\1", "i", "abc123bc"), "bc123bc,bc,bc");
    test_eq(run1_match("c|abc", "i", "abcdef"), "abc");
    test_eq(run1_match("c|abc", "i", "1234abcdef"), "abc");
    test_eq(run1_match("c|abc", "i", "abcxyz"), "abc");
    test_eq(run1_match("c|abc", "i", "abcxyzf"), "abc");
    test_eq(run1_match("c|abc", "i", "123abcdef"), "abc");
    test_eq(run1_match("c|abc", "i", "1234abcdef"), "abc");
    test_eq(run1_match("c|abc", "i", "abcdef"), "abc");
    test_eq(run1_match("c|abc", "i", "\u{83}x0abcdef"), "abc");
    test_eq(run1_match("c|abc", "i", "123abcdef"), "abc");
    test_eq(run1_match("c|abc", "i", "123abcdefC+"), "abc");
    test_eq(run1_match("c|abc", "i", "123abcdefC-"), "abc");
    test_eq(run1_match("c|abc", "i", "123abcdefC!1"), "abc");
    test_eq(run1_match("c|abc", "i", "abcabcabc"), "abc");
    test_eq(run1_match("c|abc", "i", "abcabcC!1!3"), "abc");
    test_eq(run1_match("c|abc", "i", "abcabcabcC!1!3"), "abc");
    test_eq(run1_match("c|abc", "i", "123C+"), "C");
    test_eq(run1_match("c|abc", "i", "123456C+"), "C");
    test_eq(run1_match("c|abc", "i", "123456789C+"), "C");
    test_eq(run1_match("c|abc", "i", "xyzabcC+"), "abc");
    test_eq(run1_match("c|abc", "i", "XxyzabcC+"), "abc");
    test_eq(run1_match("c|abc", "i", "abcdefC+"), "abc");
    test_eq(run1_match("c|abc", "i", "abcxyzC+"), "abc");
    test_eq(run1_match("c|abc", "i", "abbbbbcccC*1"), "c");
    test_eq(run1_match("c|abc", "i", "abbbbbcccC*1"), "c");
    test_eq(run1_match("c|abc", "i", "xbc"), "c");
    test_eq(run1_match("c|abc", "i", "abc"), "abc");
    test_eq(run1_match("c|abc", "i", "a(b)c"), "c");
    test_eq(run1_match("c|abc", "i", "a(b(c))d"), "c");
    test_eq(run1_match("c|abc", "i", "a(b(c)d"), "c");
    test_eq(run1_match("c|abc", "i", "Satan, oscillate my metallic sonatas!"), "c");
    test_eq(run1_match("c|abc", "i", "A man, a plan, a canal: Panama!"), "c");
    test_eq(run1_match("c|abc", "i", "The quick brown fox"), "c");
    test_eq(run1_match("c|abc", "i", "<abcd>"), "abc");
    test_eq(run1_match("c|abc", "i", "<abc <123> hij>"), "abc");
    test_eq(run1_match("c|abc", "i", "<abc <def> hij>"), "abc");
    test_eq(run1_match("c|abc", "i", "<abc<>def>"), "abc");
    test_eq(run1_match("c|abc", "i", "<abc<>"), "abc");
    test_eq(run1_match("c|abc", "i", "<abc"), "abc");
    test_eq(run1_match("c|abc", "i", "abcdefabc"), "abc");
    test_eq(run1_match("c|abc", "i", "a=bc"), "c");
    test_eq(run1_match("c|abc", "i", "a=bc"), "c");
    test_eq(run1_match("c|abc", "i", "acde"), "c");
    test_eq(run1_match("c|abc", "i", "Satan, oscillate my metallic sonatas!"), "c");
    test_eq(run1_match("c|abc", "i", "A man, a plan, a canal: Panama!"), "c");
    test_eq(run1_match("c|abc", "i", "The quick brown fox"), "c");
    test_eq(run1_match("(a+)*zz", "i", "zzaaCZ"), "zz,");
    test_eq(run1_match("(a+)*zz", "i", "zzaaCA"), "zz,");
    test_eq(run1_match("((w\\/|-|with)*(free|immediate)*.*?shipping\\s*[!.-]*)", "i", " Baby Bjorn Active Carrier - With free SHIPPING!!"), " Baby Bjorn Active Carrier - With free SHIPPING!!, Baby Bjorn Active Carrier - With free SHIPPING!!,,"); // "((w\\/|-|with)*(free|immediate)*.*?shipping\\s*[!.-]*)"
    test_eq(run1_match("((w\\/|-|with)*(free|immediate)*.*?shipping\\s*[!.-]*)", "i", " Baby Bjorn Active Carrier - With free SHIPPING!!"), " Baby Bjorn Active Carrier - With free SHIPPING!!, Baby Bjorn Active Carrier - With free SHIPPING!!,,"); // "((w\\/|-|with)*(free|immediate)*.*?shipping\\s*[!.-]*)"
    test_eq(run1_match("([ab]{1,4}c|xy){4,5}?123", "i", "aacaacaacaacaac123"), "aacaacaacaacaac123,aac");
    test_eq(run1_match("abcde", "i", "abcdeP"), "abcde");
    test_eq(run1_match("[abc]?123", "i", "123P"), "123");
    test_eq(run1_match("[abc]?123", "i", "c123P"), "c123");
    test_eq(run1_match("^(?:\\d){3,5}X", "i", "123X"), "123X");
    test_eq(run1_match("^(?:\\d){3,5}X", "i", "1234X"), "1234X");
    test_eq(run1_match("^(?:\\d){3,5}X", "i", "12345X"), "12345X");
    test_eq(run1_match("line\\nbreak", "i", "this is a line\nbreak"), "line\nbreak"); // "line\\nbreak"
    test_eq(run1_match("line\\nbreak", "i", "line one\nthis is a line\nbreak in the second line"), "line\nbreak"); // "line\\nbreak"
    test_eq(run1_match("line\\nbreak", "i", "this is a line\nbreak"), "line\nbreak"); // "line\\nbreak"
    test_eq(run1_match("line\\nbreak", "i", "line one\nthis is a line\nbreak in the second line"), "line\nbreak"); // "line\\nbreak"
    test_eq(run1_match("line\\nbreak", "im", "this is a line\nbreak"), "line\nbreak"); // "line\\nbreak"
    test_eq(run1_match("line\\nbreak", "im", "line one\nthis is a line\nbreak in the second line"), "line\nbreak"); // "line\\nbreak"
    test_eq(run1_match("ab.cd", "i", "ab-cd"), "ab-cd");
    test_eq(run1_match("ab.cd", "i", "ab=cd"), "ab=cd");
    test_eq(run1_match("ab.cd", "i", "ab-cd"), "ab-cd");
    test_eq(run1_match("ab.cd", "i", "ab=cd"), "ab=cd");
    test_eq(run1_match("a(b)c", "i", "abc"), "abc,b");
    test_eq(run1_match("a(b)c", "i", "abc"), "abc,b");
    test_eq(run1_match("\\s*,\\s*", "i", "\u{b},\u{b}"), "\u{b},\u{b}");
    test_eq(run1_match("\\s*,\\s*", "i", "\u{c},\u{d}"), "\u{c},\u{d}");
    test_eq(run1_match("^abc", "im", "xyz\nabc"), "abc");
    test_eq(run1_match("^abc", "im", "xyz\nabc<lf>"), "abc");
    test_eq(run1_match("^abc", "im", "xyz\u{d}\nabc<lf>"), "abc");
    test_eq(run1_match("^abc", "im", "xyz\u{d}abc<cr>"), "abc");
    test_eq(run1_match("^abc", "im", "xyz\u{d}\nabc<crlf>"), "abc");
    test_eq(run1_match("^abc", "im", "xyz\nabc<cr>"), "abc");
    test_eq(run1_match("^abc", "im", "xyz\u{d}\nabc<cr>"), "abc");
    test_eq(run1_match("^abc", "im", "xyz\nabc<crlf>"), "abc");
    test_eq(run1_match("^abc", "im", "xyz\u{d}abc<crlf>"), "abc");
    test_eq(run1_match("^abc", "im", "xyz\u{d}abc<lf>"), "abc");
    test_eq(run1_match("abc$", "im", "xyzabc"), "abc");
    test_eq(run1_match("abc$", "im", "xyzabc\n"), "abc");
    test_eq(run1_match("abc$", "im", "xyzabc\npqr"), "abc");
    test_eq(run1_match("abc$", "im", "xyzabc\u{d}<cr>"), "abc");
    test_eq(run1_match("abc$", "im", "xyzabc\u{d}pqr<cr>"), "abc");
    test_eq(run1_match("abc$", "im", "xyzabc\u{d}\n<crlf>"), "abc");
    test_eq(run1_match("abc$", "im", "xyzabc\u{d}\npqr<crlf>"), "abc");
    test_eq(run1_match("abc$", "im", "xyzabc\u{d}"), "abc");
    test_eq(run1_match("abc$", "im", "xyzabc\u{d}pqr"), "abc");
    test_eq(run1_match("abc$", "im", "xyzabc\u{d}\n"), "abc");
    test_eq(run1_match("abc$", "im", "xyzabc\u{d}\npqr"), "abc");
    test_eq(run1_match("^abc", "im", "xyz\u{d}abcdef"), "abc");
    test_eq(run1_match("^abc", "im", "xyz\nabcdef<lf>"), "abc");
    test_eq(run1_match("^abc", "im", "xyz\nabcdef"), "abc");
    test_eq(run1_match("^abc", "im", "xyz\nabcdef"), "abc");
    test_eq(run1_match("^abc", "im", "xyz\u{d}abcdef<cr>"), "abc");
    test_eq(run1_match("^abc", "im", "xyz\u{d}abcdef"), "abc");
    test_eq(run1_match("^abc", "im", "xyz\u{d}\nabcdef"), "abc");
    test_eq(run1_match("^abc", "im", "xyz\u{d}abcdef<cr>"), "abc");
    test_eq(run1_match("^abc", "im", "xyz\u{d}abcdef"), "abc");
    test_eq(run1_match("abc", "i", "xyz\u{d}abc<bad>"), "abc");
    test_eq(run1_match("abc", "i", "abc"), "abc");
    test_eq(run1_match(".*", "i", "abc\ndef"), "abc");
    test_eq(run1_match(".*", "i", "abc\u{d}def"), "abc");
    test_eq(run1_match(".*", "i", "abc\u{d}\ndef"), "abc");
    test_eq(run1_match(".*", "i", "<cr>abc\ndef"), "<cr>abc");
    test_eq(run1_match(".*", "i", "<cr>abc\u{d}def"), "<cr>abc");
    test_eq(run1_match(".*", "i", "<cr>abc\u{d}\ndef"), "<cr>abc");
    test_eq(run1_match(".*", "i", "<crlf>abc\ndef"), "<crlf>abc");
    test_eq(run1_match(".*", "i", "<crlf>abc\u{d}def"), "<crlf>abc");
    test_eq(run1_match(".*", "i", "<crlf>abc\u{d}\ndef"), "<crlf>abc");
    test_eq(run1_match("()()()()()()()()()()()()()()()()()()()()()()()()()()()()()()()()()()()()()()()()()()()()()()()()()()()()()()()()()()()()()()()()()()()()()()()()()()()()()()()()()()()()()()()()()()()()()()()()()()()()(.(.))", "i", "XYO400"), "XY,,,,,,,,,,,,,,,,,,,,,,,,,,,,,,,,,,,,,,,,,,,,,,,,,,,,,,,,,,,,,,,,,,,,,,,,,,,,,,,,,,,,,,,,,,,,,,,,,,,,,XY,Y");
    test_eq(run1_match("^a+A\\d", "", "aaaA5"), "aaaA5");
    test_eq(run1_match("^a*A\\d", "i", "aaaA5"), "aaaA5");
    test_eq(run1_match("^a*A\\d", "i", "aaaa5"), "aaaa5");
    test_eq(run1_match("a*[^a]", "", "xyCabcCxyz"), "x");
    test_eq(run1_match("a*[^a]", "", "xyCabcCxyz"), "x");
    test_eq(run1_match("a*[^a]", "", "bXaX"), "b");
    test_eq(run1_match("a*[^a]", "", "bXbX"), "b");
    test_eq(run1_match("a*[^a]", "", "** Failers"), "*");
    test_eq(run1_match("a*[^a]", "", "aXaX"), "aX");
    test_eq(run1_match("a*[^a]", "", "aXbX"), "aX");
    test_eq(run1_match("a*[^a]", "", "xx"), "x");
    test_eq(run1_match("a*[^a]", "", "xy"), "x");
    test_eq(run1_match("a*[^a]", "", "yy"), "y");
    test_eq(run1_match("a*[^a]", "", "yx"), "y");
    test_eq(run1_match("a*[^a]", "", "xx"), "x");
    test_eq(run1_match("a*[^a]", "", "xy"), "x");
    test_eq(run1_match("a*[^a]", "", "yy"), "y");
    test_eq(run1_match("a*[^a]", "", "yx"), "y");
    test_eq(run1_match("a*[^a]", "", "bxay"), "b");
    test_eq(run1_match("a*[^a]", "", "bxby"), "b");
    test_eq(run1_match("a*[^a]", "", "** Failers"), "*");
    test_eq(run1_match("a*[^a]", "", "axby"), "ax");
    test_eq(run1_match("a*[^a]", "", "XxXxxx"), "X");
    test_eq(run1_match("a*[^a]", "", "XxXyyx"), "X");
    test_eq(run1_match("a*[^a]", "", "XxXyxx"), "X");
    test_eq(run1_match("a*[^a]", "", "** Failers"), "*");
    test_eq(run1_match("a*[^a]", "", "x"), "x");
    test_eq(run1_match("a*[^a]", "", "abcabc"), "ab");
    test_eq(run1_match("^(?:(?:\\1|X)(a|b))+", "", "Xaaa"), "Xaaa,a");
    test_eq(run1_match("^(?:(?:\\1|X)(a|b))+", "", "Xaba"), "Xaba,a");
    test_eq(run1_match("(?=(\\w+))\\1:", "i", "abcd:"), "abcd:,abcd");
    test_eq(run1_match("(?=(\\w+))\\1:", "i", "abcd:"), "abcd:,abcd");
    test_eq(run1_match("(?=(\\w+))\\1:", "i", "a:aaxyz"), "a:,a");
    test_eq(run1_match("(?=(\\w+))\\1:", "i", "ab:ababxyz"), "ab:,ab");
    test_eq(run1_match("(?=(\\w+))\\1:", "i", "a:axyz"), "a:,a");
    test_eq(run1_match("(?=(\\w+))\\1:", "i", "ab:abxyz"), "ab:,ab");
    test_eq(run1_match("^a.b", "", "a\u{85}b<anycrlf> "), "a\u{85}b");
    test_eq(run1_match("^a.b", "", "a\u{85}b<any> "), "a\u{85}b");
    test_eq(run1_match("^abc.", "m", "abc1 \nabc2 \u{b}abc3xx \u{c}abc4 \u{d}abc5xx \u{d}\nabc6 \u{85}abc7 JUNK"), "abc1");
    test_eq(run1_match("abc.$", "m", "abc1\n abc2\u{b} abc3\u{c} abc4\u{d} abc5\u{d}\n abc6\u{85} abc7 abc9"), "abc1"); // "abc.$"
    // Skipping Unicode-unfriendly ^a\R*b
    // Skipping Unicode-unfriendly ^a[\R]b
    test_eq(run1_match(".+foo", "", "afoo"), "afoo");
    test_eq(run1_match(".+foo", "", "afoo"), "afoo");
    test_eq(run1_match(".+foo", "", "afoo"), "afoo");
    test_eq(run1_match(".+foo", "", "afoo"), "afoo");
    test_eq(run1_match("^$", "m", "abc\u{d}\u{d}xyz"), ""); // "^$"
    // Skipping global "^$" with string "abc\n\u{d}xyz  "
    // Skipping global "^$" with string "abc\u{d}\nxyz"
    // Skipping global "^$" with string "abc\u{d}\n\u{d}\n"
    // Skipping global "^$" with string "abc\u{d}\n\u{d}\n"
    // Skipping global "^$" with string "abc\u{d}\n\u{d}\n"
    test_eq(run1_match("abc.$", "m", "abc1\n abc2\u{b} abc3\u{c} abc4\u{d} abc5\u{d}\n abc6\u{85} abc9"), "abc1");
    test_eq(run1_match("^X", "m", "XABC"), "X");
    test_eq(run1_match("^X", "m", "XABCB"), "X");
    test_eq(run1_match("^X", "m", "XabcXabc  "), "X"); // "^X"
    // Skipping Unicode-unfriendly (foo)(\Kbar|baz)
    test_eq(run1_match("\\nA", "", "\u{d}\nA "), "\nA");
    test_eq(run1_match("[\\r\\n]A", "", "\u{d}\nA "), "\nA");
    test_eq(run1_match("(\\r|\\n)A", "", "\u{d}\nA "), "\nA,\n"); // "(\\r|\\n)A"
    // Skipping Unicode-unfriendly ^(a|b\g<1>c)
    // Skipping Unicode-unfriendly ^(a|b\g'1'c)
    // Skipping Unicode-unfriendly ^(a|b\g'-1'c)
    // Skipping Unicode-unfriendly (^(a|b\g<-1>c))
    test_eq(run1_match("(\\3)(\\1)(a)", "", "cat"), "a,,,a");
    test_eq(run1_match("(\\3)(\\1)(a)", "", "cat"), "a,,,a"); // "(\\3)(\\1)(a)"
    // Skipping Unicode-unfriendly TA]
    // Skipping Unicode-unfriendly TA]
    test_eq(run1_match("a[^]b", "", "aXb"), "aXb");
    test_eq(run1_match("a[^]b", "", "a\nb "), "a\nb");
    test_eq(run1_match("a[^]+b", "", "aXb"), "aXb");
    test_eq(run1_match("a[^]+b", "", "a\nX\nXb "), "a\nX\nXb");
    test_eq(run1_match("a.b", "", "acb"), "acb");
    test_eq(run1_match("a.b", "", "a\u{7f}b"), "a\u{7f}b");
    test_eq(run1_match("a(.*?)(.)", "", "a\u{c0}\u{88}b"), "a\u{c0},,\u{c0}");
    test_eq(run1_match("a(.*?)(.)", "", "ax{100}b"), "ax,,x");
    test_eq(run1_match("a(.*)(.)", "", "a\u{c0}\u{88}b"), "a\u{c0}\u{88}b,\u{c0}\u{88},b");
    test_eq(run1_match("a(.*)(.)", "", "ax{100}b"), "ax{100}b,x{100},b");
    test_eq(run1_match("a(.)(.)", "", "a\u{c0}\u{92}bcd"), "a\u{c0}\u{92},\u{c0},\u{92}");
    test_eq(run1_match("a(.)(.)", "", "ax{240}bcd"), "ax{,x,{");
    test_eq(run1_match("a(.?)(.)", "", "a\u{c0}\u{92}bcd"), "a\u{c0}\u{92},\u{c0},\u{92}");
    test_eq(run1_match("a(.?)(.)", "", "ax{240}bcd"), "ax{,x,{");
    test_eq(run1_match("a(.??)(.)", "", "a\u{c0}\u{92}bcd"), "a\u{c0},,\u{c0}");
    test_eq(run1_match("a(.??)(.)", "", "ax{240}bcd"), "ax,,x");
    test_eq(run1_match("a(.{3,})b", "", "ax{1234}xyb "), "ax{1234}xyb,x{1234}xy");
    test_eq(run1_match("a(.{3,})b", "", "ax{1234}x{4321}yb "), "ax{1234}x{4321}yb,x{1234}x{4321}y");
    test_eq(run1_match("a(.{3,})b", "", "ax{1234}x{4321}x{3412}b "), "ax{1234}x{4321}x{3412}b,x{1234}x{4321}x{3412}");
    test_eq(run1_match("a(.{3,})b", "", "axxxxbcdefghijb "), "axxxxbcdefghijb,xxxxbcdefghij");
    test_eq(run1_match("a(.{3,})b", "", "ax{1234}x{4321}x{3412}x{3421}b "), "ax{1234}x{4321}x{3412}x{3421}b,x{1234}x{4321}x{3412}x{3421}");
    test_eq(run1_match("a(.{3,})b", "", "ax{1234}b "), "ax{1234}b,x{1234}");
    test_eq(run1_match("a(.{3,}?)b", "", "ax{1234}xyb "), "ax{1234}xyb,x{1234}xy");
    test_eq(run1_match("a(.{3,}?)b", "", "ax{1234}x{4321}yb "), "ax{1234}x{4321}yb,x{1234}x{4321}y");
    test_eq(run1_match("a(.{3,}?)b", "", "ax{1234}x{4321}x{3412}b "), "ax{1234}x{4321}x{3412}b,x{1234}x{4321}x{3412}");
    test_eq(run1_match("a(.{3,}?)b", "", "axxxxbcdefghijb "), "axxxxb,xxxx");
    test_eq(run1_match("a(.{3,}?)b", "", "ax{1234}x{4321}x{3412}x{3421}b "), "ax{1234}x{4321}x{3412}x{3421}b,x{1234}x{4321}x{3412}x{3421}");
    test_eq(run1_match("a(.{3,}?)b", "", "ax{1234}b "), "ax{1234}b,x{1234}");
    test_eq(run1_match("a(.{3,5})b", "", "axxxxbcdefghijb "), "axxxxb,xxxx");
    test_eq(run1_match("a(.{3,5})b", "", "axbxxbcdefghijb "), "axbxxb,xbxx");
    test_eq(run1_match("a(.{3,5})b", "", "axxxxxbcdefghijb "), "axxxxxb,xxxxx");
    test_eq(run1_match("a(.{3,5}?)b", "", "axxxxbcdefghijb "), "axxxxb,xxxx");
    test_eq(run1_match("a(.{3,5}?)b", "", "axbxxbcdefghijb "), "axbxxb,xbxx");
    test_eq(run1_match("a(.{3,5}?)b", "", "axxxxxbcdefghijb "), "axxxxxb,xxxxx"); // "a(.{3,5}?)b"
    // Skipping Unicode-unfriendly X\C*
    // Skipping Unicode-unfriendly X\C*?
    test_eq(tc.compile("[^a]+").run_global_match("bcd"), "bcd"); // "[^a]+"
    // Skipping Unicode-unfriendly [^a]+
    test_eq(run1_match("^[^a]{2}", "", "x{100}bc"), "x{");
    test_eq(run1_match("^[^a]{2,}", "", "x{100}bcAa"), "x{100}bcA");
    test_eq(run1_match("^[^a]{2,}?", "", "x{100}bca"), "x{");
    test_eq(tc.compilef("[^a]+", "i").run_global_match("bcd"), "bcd"); // "[^a]+"
    // Skipping Unicode-unfriendly [^a]+
    test_eq(run1_match("^[^a]{2}", "i", "x{100}bc"), "x{");
    test_eq(run1_match("^[^a]{2,}", "i", "x{100}bcAa"), "x{100}bc");
    test_eq(run1_match("^[^a]{2,}?", "i", "x{100}bca"), "x{");
    test_eq(run1_match("^[^a]{2,}?", "i", "x{100}x{100} "), "x{");
    test_eq(run1_match("^[^a]{2,}?", "i", "x{100}x{100} "), "x{");
    test_eq(run1_match("^[^a]{2,}?", "i", "x{100}x{100}x{100}x{100} "), "x{");
    test_eq(run1_match("^[^a]{2,}?", "i", "x{100}x{100}x{100}x{100} "), "x{");
    test_eq(run1_match("^[^a]{2,}?", "i", "Xyyyax{100}x{100}bXzzz"), "Xy");
    test_eq(run1_match("\\D", "", "1X2"), "X");
    test_eq(run1_match("\\D", "", "1x{100}2 "), "x");
    test_eq(run1_match(">\\S", "", "> >X Y"), ">X");
    test_eq(run1_match(">\\S", "", "> >x{100} Y"), ">x");
    test_eq(run1_match("\\d", "", "x{100}3"), "1");
    test_eq(run1_match("\\s", "", "x{100} X"), " ");
    test_eq(run1_match("\\D+", "", "12abcd34"), "abcd");
    test_eq(run1_match("\\D+", "", "*** Failers"), "*** Failers");
    test_eq(run1_match("\\D+", "", "1234  "), "  ");
    test_eq(run1_match("\\D{2,3}", "", "12abcd34"), "abc");
    test_eq(run1_match("\\D{2,3}", "", "12ab34"), "ab");
    test_eq(run1_match("\\D{2,3}", "", "*** Failers  "), "***");
    test_eq(run1_match("\\D{2,3}", "", "12a34  "), "  ");
    test_eq(run1_match("\\D{2,3}?", "", "12abcd34"), "ab");
    test_eq(run1_match("\\D{2,3}?", "", "12ab34"), "ab");
    test_eq(run1_match("\\D{2,3}?", "", "*** Failers  "), "**");
    test_eq(run1_match("\\D{2,3}?", "", "12a34  "), "  ");
    test_eq(run1_match("\\d+", "", "12abcd34"), "12");
    test_eq(run1_match("\\d{2,3}", "", "12abcd34"), "12");
    test_eq(run1_match("\\d{2,3}", "", "1234abcd"), "123");
    test_eq(run1_match("\\d{2,3}?", "", "12abcd34"), "12");
    test_eq(run1_match("\\d{2,3}?", "", "1234abcd"), "12");
    test_eq(run1_match("\\S+", "", "12abcd34"), "12abcd34");
    test_eq(run1_match("\\S+", "", "*** Failers"), "***");
    test_eq(run1_match("\\S{2,3}", "", "12abcd34"), "12a");
    test_eq(run1_match("\\S{2,3}", "", "1234abcd"), "123");
    test_eq(run1_match("\\S{2,3}", "", "*** Failers"), "***");
    test_eq(run1_match("\\S{2,3}?", "", "12abcd34"), "12");
    test_eq(run1_match("\\S{2,3}?", "", "1234abcd"), "12");
    test_eq(run1_match("\\S{2,3}?", "", "*** Failers"), "**");
    test_eq(run1_match(">\\s+<", "", "12>      <34"), ">      <");
    test_eq(run1_match(">\\s{2,3}<", "", "ab>  <cd"), ">  <");
    test_eq(run1_match(">\\s{2,3}<", "", "ab>   <ce"), ">   <");
    test_eq(run1_match(">\\s{2,3}?<", "", "ab>  <cd"), ">  <");
    test_eq(run1_match(">\\s{2,3}?<", "", "ab>   <ce"), ">   <");
    test_eq(run1_match("\\w+", "", "12      34"), "12");
    test_eq(run1_match("\\w+", "", "*** Failers"), "Failers");
    test_eq(run1_match("\\w{2,3}", "", "ab  cd"), "ab");
    test_eq(run1_match("\\w{2,3}", "", "abcd ce"), "abc");
    test_eq(run1_match("\\w{2,3}", "", "*** Failers"), "Fai");
    test_eq(run1_match("\\w{2,3}?", "", "ab  cd"), "ab");
    test_eq(run1_match("\\w{2,3}?", "", "abcd ce"), "ab");
    test_eq(run1_match("\\w{2,3}?", "", "*** Failers"), "Fa");
    test_eq(run1_match("\\W+", "", "12====34"), "====");
    test_eq(run1_match("\\W+", "", "*** Failers"), "*** ");
    test_eq(run1_match("\\W+", "", "abcd "), " ");
    test_eq(run1_match("\\W{2,3}", "", "ab====cd"), "===");
    test_eq(run1_match("\\W{2,3}", "", "ab==cd"), "==");
    test_eq(run1_match("\\W{2,3}", "", "*** Failers"), "***");
    test_eq(run1_match("\\W{2,3}?", "", "ab====cd"), "==");
    test_eq(run1_match("\\W{2,3}?", "", "ab==cd"), "==");
    test_eq(run1_match("\\W{2,3}?", "", "*** Failers"), "**");
    test_eq(run1_match("\\W{2,3}?", "", "*** Failers "), "**");
    test_eq(run1_match("\\W{2,3}?", "", "*** Failers "), "**");
    test_eq(run1_match("\\W{2,3}?", "", "*** Failers"), "**");
    test_eq(run1_match("\\W{2,3}?", "", "X  "), "  ");
    test_eq(run1_match("\\W{2,3}?", "", "*** Failers"), "**");
    test_eq(run1_match("\\W{2,3}?", "", "X  "), "  ");
    test_eq(run1_match("\\W{2,3}?", "", "*** Failers"), "**");
    test_eq(run1_match("\\W{2,3}?", "", "X  "), "  ");
    test_eq(run1_match("\\W{2,3}?", "", "*** Failers"), "**");
    test_eq(run1_match("\\W{2,3}?", "", "x{200}X   "), "  ");
    test_eq(run1_match("\\W{2,3}?", "", "*** Failers"), "**");
    test_eq(run1_match("\\W{2,3}?", "", "x{200}X   "), "  ");
    test_eq(run1_match("\\W{2,3}?", "", "*** Failers"), "**");
    test_eq(run1_match("\\W{2,3}?", "", "x{200}X   "), "  ");
    test_eq(run1_match("[\\xFF]", "", ">\u{ff}<"), "\u{ff}");
    test_eq(run1_match("[^\\xFF]", "", "XYZ"), "X");
    test_eq(run1_match("[^\\xff]", "", "XYZ"), "X");
    test_eq(run1_match("[^\\xff]", "", "x{123} "), "x");
    test_eq(run1_match("(|a)", "", "catac"), ","); // "(|a)"
    // Skipping global "(|a)" with string "ax{256}a "
    // Skipping global "(|a)" with string "x{85}"
    // Skipping global "(|a)" with string "\u{1234} "
    // Skipping global "(|a)" with string "\u{1234} "
    // Skipping global "(|a)" with string "abcdefg"
    // Skipping global "(|a)" with string "ab"
    // Skipping global "(|a)" with string "a "
    test_eq(run1_match("\\S\\S", "", "Ax{a3}BC"), "Ax");
    test_eq(run1_match("\\S{2}", "", "Ax{a3}BC"), "Ax");
    test_eq(run1_match("\\W\\W", "", "+x{a3}== "), "}=");
    test_eq(run1_match("\\W{2}", "", "+x{a3}== "), "}=");
    test_eq(run1_match("\\S", "", "x{442}x{435}x{441}x{442}"), "x");
    test_eq(run1_match("[\\S]", "", "x{442}x{435}x{441}x{442}"), "x");
    test_eq(run1_match("\\D", "", "x{442}x{435}x{441}x{442}"), "x");
    test_eq(run1_match("[\\D]", "", "x{442}x{435}x{441}x{442}"), "x");
    test_eq(run1_match("\\W", "", "x{2442}x{2435}x{2441}x{2442}"), "{");
    test_eq(run1_match("[\\W]", "", "x{2442}x{2435}x{2441}x{2442}"), "{");
    test_eq(run1_match("[\\S\\s]*", "", "abc\n\u{d}x{442}x{435}x{441}x{442}xyz "), "abc\n\u{d}x{442}x{435}x{441}x{442}xyz ");
    test_eq(run1_match("[\\S\\s]*", "", "x{442}x{435}x{441}x{442}"), "x{442}x{435}x{441}x{442}");
    test_eq(run1_match(".[^\\S].", "", "abc defx{442}x{443}xyz\npqr"), "c d");
    test_eq(run1_match(".[^\\S\\n].", "", "abc defx{442}x{443}xyz\npqr"), "c d");
    test_eq(run1_match("^[^d]*?$", "", "abc"), "abc");
    test_eq(run1_match("^[^d]*?$", "", "abc"), "abc");
    test_eq(run1_match("^[^d]*?$", "i", "abc"), "abc");
    test_eq(run1_match("^[^d]*?$", "i", "abc"), "abc");
    test_eq(run1_match(".{3,5}X", "", "x{212ab}x{212ab}x{212ab}x{861}X"), "{861}X");
    test_eq(run1_match(".{3,5}?", "", "x{212ab}x{212ab}x{212ab}x{861}"), "x{2");
    test_eq(run1_match(".{3,5}?", "", "x{c0}b"), "x{c");
    test_eq(run1_match(".{3,5}?", "", "ax{c0}aaaa/ "), "ax{");
    test_eq(run1_match(".{3,5}?", "", "ax{c0}aaaa/ "), "ax{");
    test_eq(run1_match(".{3,5}?", "", "ax{c0}ax{c0}aaa/ "), "ax{");
    test_eq(run1_match(".{3,5}?", "", "ax{c0}aaaa/ "), "ax{");
    test_eq(run1_match(".{3,5}?", "", "ax{c0}ax{c0}aaa/ "), "ax{");
    test_eq(run1_match(".{3,5}?", "", "ax{c0}aaaa/ "), "ax{");
    test_eq(run1_match(".{3,5}?", "", "ax{c0}ax{c0}aaa/ "), "ax{");
    test_eq(run1_match(".{3,5}?", "", "Should produce an error diagnostic"), "Sho");
    test_eq(run1_match("^[ab]", "", "bar"), "b");
    test_eq(run1_match("^[^ab]", "", "c"), "c");
    test_eq(run1_match("^[^ab]", "", "x{ff}"), "x");
    test_eq(run1_match("^[^ab]", "", "x{100}  "), "x");
    test_eq(run1_match("^[^ab]", "", "*** Failers "), "*");
    test_eq(run1_match("[^ab\\xC0-\\xF0]", "", "x{f1}"), "x");
    test_eq(run1_match("[^ab\\xC0-\\xF0]", "", "x{bf}"), "x");
    test_eq(run1_match("[^ab\\xC0-\\xF0]", "", "x{100}"), "x");
    test_eq(run1_match("[^ab\\xC0-\\xF0]", "", "x{1000}   "), "x");
    test_eq(run1_match("[^ab\\xC0-\\xF0]", "", "*** Failers"), "*");
    test_eq(run1_match("[^ab\\xC0-\\xF0]", "", "x{c0} "), "x");
    test_eq(run1_match("[^ab\\xC0-\\xF0]", "", "x{f0} "), "x");
    test_eq(run1_match("[^ab\\xC0-\\xF0]", "", "1234"), "1");
    test_eq(run1_match("[^ab\\xC0-\\xF0]", "", "\"1234\" "), "\"");
    test_eq(run1_match("[^ab\\xC0-\\xF0]", "", "x{100}1234"), "x");
    test_eq(run1_match("[^ab\\xC0-\\xF0]", "", "\"x{100}1234\"  "), "\"");
    test_eq(run1_match("[^ab\\xC0-\\xF0]", "", "x{100}x{100}12ab "), "x");
    test_eq(run1_match("[^ab\\xC0-\\xF0]", "", "x{100}x{100}\"12\" "), "x");
    test_eq(run1_match("[^ab\\xC0-\\xF0]", "", "*** Failers "), "*");
    test_eq(run1_match("[^ab\\xC0-\\xF0]", "", "x{100}x{100}abcd"), "x");
    test_eq(run1_match("[^ab\\xC0-\\xF0]", "", "A"), "A");
    test_eq(run1_match("[^ab\\xC0-\\xF0]", "", "x{100}"), "x");
    test_eq(run1_match("[^ab\\xC0-\\xF0]", "", "Zx{100}"), "Z");
    test_eq(run1_match("[^ab\\xC0-\\xF0]", "", "x{100}Z"), "x");
    test_eq(run1_match("[^ab\\xC0-\\xF0]", "", "*** Failers "), "*");
    test_eq(run1_match("[^ab\\xC0-\\xF0]", "", "Zx{100}"), "Z");
    test_eq(run1_match("[^ab\\xC0-\\xF0]", "", "x{100}"), "x");
    test_eq(run1_match("[^ab\\xC0-\\xF0]", "", "x{100}Z"), "x");
    test_eq(run1_match("[^ab\\xC0-\\xF0]", "", "*** Failers "), "*");
    test_eq(run1_match("[^ab\\xC0-\\xF0]", "", "x{100}"), "x");
    test_eq(run1_match("[^ab\\xC0-\\xF0]", "", "x{104}"), "x");
    test_eq(run1_match("[^ab\\xC0-\\xF0]", "", "*** Failers"), "*");
    test_eq(run1_match("[^ab\\xC0-\\xF0]", "", "x{105}"), "x");
    test_eq(run1_match("[^ab\\xC0-\\xF0]", "", "x{ff}    "), "x");
    test_eq(run1_match("[^ab\\xC0-\\xF0]", "", "x{100}"), "x");
    test_eq(run1_match("[^ab\\xC0-\\xF0]", "", "\u{100} "), "\u{100}");
    test_eq(run1_match("[\\xFF]", "", ">\u{ff}<"), "\u{ff}");
    test_eq(run1_match("[^\\xff]", "", "\u{d6} # Matches without Study"), "\u{d6}");
    test_eq(run1_match("[^\\xff]", "", "x{d6}"), "x");
    test_eq(run1_match("[^\\xff]", "", "\u{d6} <-- Same with Study"), "\u{d6}");
    test_eq(run1_match("[^\\xff]", "", "x{d6}"), "x");
    test_eq(run1_match("[^\\xff]", "", "\u{d6} # Matches without Study"), "\u{d6}");
    test_eq(run1_match("[^\\xff]", "", "x{d6} "), "x");
    test_eq(run1_match("[^\\xff]", "", "\u{d6} <-- Same with Study"), "\u{d6}");
    test_eq(run1_match("[^\\xff]", "", "x{d6} "), "x");
    test_eq(run1_match("[^\\xff]", "", "\u{fffd}]"), "\u{fffd}");
    test_eq(run1_match("[^\\xff]", "", "\u{fffd}"), "\u{fffd}");
    test_eq(run1_match("[^\\xff]", "", "\u{fffd}\u{fffd}\u{fffd}"), "\u{fffd}");
    test_eq(run1_match("[^\\xff]", "", "\u{fffd}\u{fffd}\u{fffd}?"), "\u{fffd}");
    test_eq(run1_match("\\W", "", "A.B"), ".");
    test_eq(run1_match("\\W", "", "Ax{100}B "), "{");
    test_eq(run1_match("\\w", "", "x{100}X   "), "x");
    test_eq(run1_match("\\w", "", "ax{1234}b"), "a");
    test_eq(run1_match("^abc.", "m", "abc1 \nabc2 \u{b}abc3xx \u{c}abc4 \u{d}abc5xx \u{d}\nabc6 x{0085}abc7 x{2028}abc8 x{2029}abc9 JUNK"), "abc1");
    test_eq(run1_match("abc.$", "m", "abc1\n abc2\u{b} abc3\u{c} abc4\u{d} abc5\u{d}\n abc6x{0085} abc7x{2028} abc8x{2029} abc9"), "abc1"); // "abc.$"
    // Skipping Unicode-unfriendly ^a\R*b
    test_eq(run1_match(".*$", "", "x{1ec5} "), "x{1ec5} ");
    test_eq(run1_match(".*a.*=.b.*", "", "QQQx{2029}ABCaXYZ=!bPQR"), "QQQx{2029}ABCaXYZ=!bPQR");
    test_eq(run1_match("a[^]b", "", "a\nb "), "a\nb");
    test_eq(run1_match("a[^]+b", "", "aXb"), "aXb");
    test_eq(run1_match("a[^]+b", "", "a\nX\nXx{1234}b "), "a\nX\nXx{1234}b");
    test_eq(run1_match("X", "", "Ax{1ec5}ABCXYZ"), "X"); // "X"
    // Skipping Unicode-unfriendly [\p{Nd}]
    // Skipping Unicode-unfriendly [\p{Nd}]
    // Skipping Unicode-unfriendly [\P{Nd}]+
    test_eq(run1_match("\\D+", "", "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"), "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
    test_eq(run1_match("\\D+", "", " "), " ");
    test_eq(run1_match("\\D+", "", "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"), "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
    test_eq(run1_match("[\\D]+", "", "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"), "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
    test_eq(run1_match("[\\D\\P{Nd}]+", "", "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"), "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"); // "[\\D\\P{Nd}]+"
    // Skipping Unicode-unfriendly ^[\X]
    // Skipping Unicode-unfriendly ^(\X*)(.)
    // Skipping Unicode-unfriendly ^(\X*)(.)
    // Skipping Unicode-unfriendly ^(\X*?)(.)
    // Skipping Unicode-unfriendly ^(\X*?)(.)
    test_eq(run1_match("^[\\p{Any}]X", "", "AXYZ"), "AX"); // "^[\\p{Any}]X"
    // Skipping Unicode-unfriendly ^[\P{Any}]X
    test_eq(run1_match("^[\\p{Any}]?X", "", "XYZ"), "X");
    test_eq(run1_match("^[\\p{Any}]?X", "", "AXYZ"), "AX");
    test_eq(run1_match("^[\\P{Any}]?X", "", "XYZ"), "X"); // "^[\\P{Any}]?X"
    // Skipping Unicode-unfriendly ^[\P{Any}]?X
    test_eq(run1_match("^[\\p{Any}]+X", "", "AXYZ"), "AX"); // "^[\\p{Any}]+X"
    // Skipping Unicode-unfriendly ^[\P{Any}]+X
    test_eq(run1_match("^[\\p{Any}]*X", "", "XYZ"), "X");
    test_eq(run1_match("^[\\p{Any}]*X", "", "AXYZ"), "AX");
    test_eq(run1_match("^[\\P{Any}]*X", "", "XYZ"), "X"); // "^[\\P{Any}]*X"
    // Skipping Unicode-unfriendly ^[\P{Any}]*X
    // Skipping Unicode-unfriendly ([\pL]=(abc))*X
    test_eq(run1_match("(A)\\1", "i", "AA"), "AA,A");
    test_eq(run1_match("(A)\\1", "i", "Aa"), "Aa,A");
    test_eq(run1_match("(A)\\1", "i", "aa"), "aa,a");
    test_eq(run1_match("(A)\\1", "i", "aA"), "aA,a");
    test_eq(run1_match("abc", "", "abc"), "abc");
    test_eq(run1_match("ab*c", "", "abc"), "abc");
    test_eq(run1_match("ab*c", "", "abbbbc"), "abbbbc");
    test_eq(run1_match("ab*c", "", "ac"), "ac");
    test_eq(run1_match("ab+c", "", "abc"), "abc");
    test_eq(run1_match("ab+c", "", "abbbbbbc"), "abbbbbbc");
    test_eq(run1_match("a*", "", "a"), "a");
    test_eq(run1_match("a*", "", "aaaaaaaaaaaaaaaaa"), "aaaaaaaaaaaaaaaaa");
    test_eq(run1_match("a*", "", "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaa "), "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
    test_eq(run1_match("a*", "", "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaF "), "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
    test_eq(run1_match("(a|abcd|african)", "", "a"), "a,a"); // "(a|abcd|african)"
    test_eq(run1_match("(a|abcd|african)", "", "abcd"), "a,a"); // "(a|abcd|african)"
    test_eq(run1_match("(a|abcd|african)", "", "african"), "a,a"); // "(a|abcd|african)"
    test_eq(run1_match("^abc", "", "abcdef"), "abc");
    test_eq(run1_match("^abc", "m", "abcdef"), "abc");
    test_eq(run1_match("^abc", "m", "xyz\nabc    "), "abc");
    test_eq(run1_match("x\\dy\\Dz", "", "x9yzz"), "x9yzz");
    test_eq(run1_match("x\\dy\\Dz", "", "x0y+z"), "x0y+z");
    test_eq(run1_match("x\\sy\\Sz", "", "x yzz"), "x yzz");
    test_eq(run1_match("x\\sy\\Sz", "", "x y+z"), "x y+z");
    test_eq(run1_match("x\\wy\\Wz", "", "xxy+z"), "xxy+z");
    test_eq(run1_match("x.y", "", "x+y"), "x+y");
    test_eq(run1_match("x.y", "", "x-y"), "x-y");
    test_eq(run1_match("x.y", "", "x+y"), "x+y");
    test_eq(run1_match("x.y", "", "x-y"), "x-y");
    test_eq(run1_match("a\\d$", "", "ba0"), "a0");
    test_eq(run1_match("a\\d$", "m", "ba0"), "a0");
    test_eq(run1_match("a\\d$", "m", "ba0\n"), "a0");
    test_eq(run1_match("a\\d$", "m", "ba0\ncd   "), "a0");
    test_eq(run1_match("abc", "i", "abc"), "abc");
    test_eq(run1_match("abc", "i", "aBc"), "aBc");
    test_eq(run1_match("abc", "i", "ABC"), "ABC");
    test_eq(run1_match("[^a]", "", "abcd"), "b");
    test_eq(run1_match("ab?\\w", "", "abz"), "abz");
    test_eq(run1_match("ab?\\w", "", "abbz"), "abb");
    test_eq(run1_match("ab?\\w", "", "azz  "), "az");
    test_eq(run1_match("x{0,3}yz", "", "ayzq"), "yz");
    test_eq(run1_match("x{0,3}yz", "", "axyzq"), "xyz");
    test_eq(run1_match("x{0,3}yz", "", "axxyz"), "xxyz");
    test_eq(run1_match("x{0,3}yz", "", "axxxyzq"), "xxxyz");
    test_eq(run1_match("x{0,3}yz", "", "axxxxyzq"), "xxxyz");
    test_eq(run1_match("x{3}yz", "", "axxxyzq"), "xxxyz");
    test_eq(run1_match("x{3}yz", "", "axxxxyzq"), "xxxyz");
    test_eq(run1_match("x{2,3}yz", "", "axxyz"), "xxyz");
    test_eq(run1_match("x{2,3}yz", "", "axxxyzq"), "xxxyz");
    test_eq(run1_match("x{2,3}yz", "", "axxxxyzq"), "xxxyz");
    test_eq(run1_match("[^a]+", "", "bac"), "b");
    test_eq(run1_match("[^a]+", "", "bcdefax"), "bcdef");
    test_eq(run1_match("[^a]+", "", "*** Failers"), "*** F");
    test_eq(run1_match("[^a]+", "", "aaaaa   "), "   ");
    test_eq(run1_match("[^a]*", "", "bac"), "b");
    test_eq(run1_match("[^a]*", "", "bcdefax"), "bcdef");
    test_eq(run1_match("[^a]*", "", "*** Failers"), "*** F");
    test_eq(run1_match("[^a]*", "", "aaaaa   "), "");
    test_eq(run1_match("[^a]{3,5}", "", "xyz"), "xyz");
    test_eq(run1_match("[^a]{3,5}", "", "awxyza"), "wxyz");
    test_eq(run1_match("[^a]{3,5}", "", "abcdefa"), "bcdef");
    test_eq(run1_match("[^a]{3,5}", "", "abcdefghijk"), "bcdef");
    test_eq(run1_match("[^a]{3,5}", "", "*** Failers"), "*** F");
    test_eq(run1_match("[^a]{3,5}", "", "aaaaa         "), "     ");
    test_eq(run1_match("\\d*", "", "1234b567"), "1234");
    test_eq(run1_match("\\d*", "", "xyz"), "");
    test_eq(run1_match("\\D*", "", "a1234b567"), "a");
    test_eq(run1_match("\\D*", "", "xyz"), "xyz");
    test_eq(run1_match("\\D*", "", " "), " ");
    test_eq(run1_match("\\d+", "", "ab1234c56"), "1234");
    test_eq(run1_match("\\D+", "", "ab123c56"), "ab");
    test_eq(run1_match("\\D+", "", "*** Failers"), "*** Failers");
    test_eq(run1_match("\\d?A", "", "045ABC"), "5A");
    test_eq(run1_match("\\d?A", "", "ABC"), "A");
    test_eq(run1_match("\\D?A", "", "ABC"), "A");
    test_eq(run1_match("\\D?A", "", "BAC"), "BA");
    test_eq(run1_match("\\D?A", "", "9ABC             "), "A");
    test_eq(run1_match("a+", "", "aaaa"), "aaaa");
    test_eq(run1_match("^.*xyz", "", "xyz"), "xyz");
    test_eq(run1_match("^.*xyz", "", "ggggggggxyz"), "ggggggggxyz");
    test_eq(run1_match("^.+xyz", "", "abcdxyz"), "abcdxyz");
    test_eq(run1_match("^.+xyz", "", "axyz"), "axyz");
    test_eq(run1_match("^.?xyz", "", "xyz"), "xyz");
    test_eq(run1_match("^.?xyz", "", "cxyz       "), "cxyz");
    test_eq(run1_match("^\\d{2,3}X", "", "12X"), "12X");
    test_eq(run1_match("^\\d{2,3}X", "", "123X"), "123X");
    test_eq(run1_match("^[abcd]\\d", "", "a45"), "a4");
    test_eq(run1_match("^[abcd]\\d", "", "b93"), "b9");
    test_eq(run1_match("^[abcd]\\d", "", "c99z"), "c9");
    test_eq(run1_match("^[abcd]\\d", "", "d04"), "d0");
    test_eq(run1_match("^[abcd]*\\d", "", "a45"), "a4");
    test_eq(run1_match("^[abcd]*\\d", "", "b93"), "b9");
    test_eq(run1_match("^[abcd]*\\d", "", "c99z"), "c9");
    test_eq(run1_match("^[abcd]*\\d", "", "d04"), "d0");
    test_eq(run1_match("^[abcd]*\\d", "", "abcd1234"), "abcd1");
    test_eq(run1_match("^[abcd]*\\d", "", "1234  "), "1");
    test_eq(run1_match("^[abcd]+\\d", "", "a45"), "a4");
    test_eq(run1_match("^[abcd]+\\d", "", "b93"), "b9");
    test_eq(run1_match("^[abcd]+\\d", "", "c99z"), "c9");
    test_eq(run1_match("^[abcd]+\\d", "", "d04"), "d0");
    test_eq(run1_match("^[abcd]+\\d", "", "abcd1234"), "abcd1");
    test_eq(run1_match("^a+X", "", "aX"), "aX");
    test_eq(run1_match("^a+X", "", "aaX "), "aaX");
    test_eq(run1_match("^[abcd]?\\d", "", "a45"), "a4");
    test_eq(run1_match("^[abcd]?\\d", "", "b93"), "b9");
    test_eq(run1_match("^[abcd]?\\d", "", "c99z"), "c9");
    test_eq(run1_match("^[abcd]?\\d", "", "d04"), "d0");
    test_eq(run1_match("^[abcd]?\\d", "", "1234  "), "1");
    test_eq(run1_match("^[abcd]{2,3}\\d", "", "ab45"), "ab4");
    test_eq(run1_match("^[abcd]{2,3}\\d", "", "bcd93"), "bcd9");
    test_eq(run1_match("^(abc)*\\d", "", "abc45"), "abc4,abc");
    test_eq(run1_match("^(abc)*\\d", "", "abcabcabc45"), "abcabcabc4,abc");
    test_eq(run1_match("^(abc)*\\d", "", "42xyz "), "4,");
    test_eq(run1_match("^(abc)+\\d", "", "abc45"), "abc4,abc");
    test_eq(run1_match("^(abc)+\\d", "", "abcabcabc45"), "abcabcabc4,abc");
    test_eq(run1_match("^(abc)?\\d", "", "abc45"), "abc4,abc");
    test_eq(run1_match("^(abc)?\\d", "", "42xyz "), "4,");
    test_eq(run1_match("^(abc){2,3}\\d", "", "abcabc45"), "abcabc4,abc");
    test_eq(run1_match("^(abc){2,3}\\d", "", "abcabcabc45"), "abcabcabc4,abc");
    test_eq(run1_match("^(a*\\w|ab)=(a*\\w|ab)", "", "ab=ab"), "ab=ab,ab,ab");
    test_eq(run1_match("^(a*\\w|ab)=(a*\\w|ab)", "", "ab=ab"), "ab=ab,ab,ab");
    test_eq(run1_match("^abc", "", "abcdef"), "abc");
    test_eq(run1_match("^abc", "", "abcdefB  "), "abc");
    test_eq(run1_match("^(a*|xyz)", "", "bcd"), ",");
    test_eq(run1_match("^(a*|xyz)", "", "aaabcd"), "aaa,aaa");
    test_eq(run1_match("^(a*|xyz)", "", "xyz"), ",");
    test_eq(run1_match("^(a*|xyz)", "", "xyzN  "), ",");
    test_eq(run1_match("^(a*|xyz)", "", "*** Failers"), ",");
    test_eq(run1_match("^(a*|xyz)", "", "bcdN   "), ",");
    test_eq(run1_match("xyz$", "", "xyz"), "xyz");
    test_eq(run1_match("xyz$", "m", "xyz"), "xyz");
    test_eq(run1_match("xyz$", "m", "xyz\n "), "xyz");
    test_eq(run1_match("xyz$", "m", "abcxyz\npqr "), "xyz");
    test_eq(run1_match("xyz$", "m", "abcxyz\npqrZ "), "xyz");
    test_eq(run1_match("xyz$", "m", "xyz\nZ    "), "xyz");
    test_eq(run1_match("^abcdef", "", "abcdefP"), "abcdef");
    test_eq(run1_match("^a{2,4}\\d+z", "", "aa0zP"), "aa0z");
    test_eq(run1_match("^a{2,4}\\d+z", "", "aaaa4444444444444zP "), "aaaa4444444444444z");
    test_eq(run1_match("the quick brown fox", "", "the quick brown fox"), "the quick brown fox"); // "the quick brown fox"
    test_eq(run1_match("the quick brown fox", "", "What do you know about the quick brown fox?"), "the quick brown fox"); // "the quick brown fox"
    test_eq(run1_match("The quick brown fox", "i", "the quick brown fox"), "the quick brown fox"); // "The quick brown fox"
    test_eq(run1_match("The quick brown fox", "i", "The quick brown FOX"), "The quick brown FOX"); // "The quick brown fox"
    test_eq(run1_match("The quick brown fox", "i", "What do you know about the quick brown fox?"), "the quick brown fox"); // "The quick brown fox"
    test_eq(run1_match("The quick brown fox", "i", "What do you know about THE QUICK BROWN FOX?"), "THE QUICK BROWN FOX"); // "The quick brown fox"
    // Skipping Unicode-unfriendly abcd\t\n\r\f\a\e\071\x3b\$\\\?caxyz
    test_eq(run1_match("a*abc?xyz+pqr{3}ab{2,}xy{4,5}pq{0,6}AB{0,}zz", "", "abxyzpqrrrabbxyyyypqAzz"), "abxyzpqrrrabbxyyyypqAzz"); // "a*abc?xyz+pqr{3}ab{2,}xy{4,5}pq{0,6}AB{0,}zz"
    test_eq(run1_match("a*abc?xyz+pqr{3}ab{2,}xy{4,5}pq{0,6}AB{0,}zz", "", "abxyzpqrrrabbxyyyypqAzz"), "abxyzpqrrrabbxyyyypqAzz"); // "a*abc?xyz+pqr{3}ab{2,}xy{4,5}pq{0,6}AB{0,}zz"
    test_eq(run1_match("a*abc?xyz+pqr{3}ab{2,}xy{4,5}pq{0,6}AB{0,}zz", "", "aabxyzpqrrrabbxyyyypqAzz"), "aabxyzpqrrrabbxyyyypqAzz"); // "a*abc?xyz+pqr{3}ab{2,}xy{4,5}pq{0,6}AB{0,}zz"
    test_eq(run1_match("a*abc?xyz+pqr{3}ab{2,}xy{4,5}pq{0,6}AB{0,}zz", "", "aaabxyzpqrrrabbxyyyypqAzz"), "aaabxyzpqrrrabbxyyyypqAzz"); // "a*abc?xyz+pqr{3}ab{2,}xy{4,5}pq{0,6}AB{0,}zz"
    test_eq(run1_match("a*abc?xyz+pqr{3}ab{2,}xy{4,5}pq{0,6}AB{0,}zz", "", "aaaabxyzpqrrrabbxyyyypqAzz"), "aaaabxyzpqrrrabbxyyyypqAzz"); // "a*abc?xyz+pqr{3}ab{2,}xy{4,5}pq{0,6}AB{0,}zz"
    test_eq(run1_match("a*abc?xyz+pqr{3}ab{2,}xy{4,5}pq{0,6}AB{0,}zz", "", "abcxyzpqrrrabbxyyyypqAzz"), "abcxyzpqrrrabbxyyyypqAzz"); // "a*abc?xyz+pqr{3}ab{2,}xy{4,5}pq{0,6}AB{0,}zz"
    test_eq(run1_match("a*abc?xyz+pqr{3}ab{2,}xy{4,5}pq{0,6}AB{0,}zz", "", "aabcxyzpqrrrabbxyyyypqAzz"), "aabcxyzpqrrrabbxyyyypqAzz"); // "a*abc?xyz+pqr{3}ab{2,}xy{4,5}pq{0,6}AB{0,}zz"
    test_eq(run1_match("a*abc?xyz+pqr{3}ab{2,}xy{4,5}pq{0,6}AB{0,}zz", "", "aaabcxyzpqrrrabbxyyyypAzz"), "aaabcxyzpqrrrabbxyyyypAzz"); // "a*abc?xyz+pqr{3}ab{2,}xy{4,5}pq{0,6}AB{0,}zz"
    test_eq(run1_match("a*abc?xyz+pqr{3}ab{2,}xy{4,5}pq{0,6}AB{0,}zz", "", "aaabcxyzpqrrrabbxyyyypqAzz"), "aaabcxyzpqrrrabbxyyyypqAzz"); // "a*abc?xyz+pqr{3}ab{2,}xy{4,5}pq{0,6}AB{0,}zz"
    test_eq(run1_match("a*abc?xyz+pqr{3}ab{2,}xy{4,5}pq{0,6}AB{0,}zz", "", "aaabcxyzpqrrrabbxyyyypqqAzz"), "aaabcxyzpqrrrabbxyyyypqqAzz"); // "a*abc?xyz+pqr{3}ab{2,}xy{4,5}pq{0,6}AB{0,}zz"
    test_eq(run1_match("a*abc?xyz+pqr{3}ab{2,}xy{4,5}pq{0,6}AB{0,}zz", "", "aaabcxyzpqrrrabbxyyyypqqqAzz"), "aaabcxyzpqrrrabbxyyyypqqqAzz"); // "a*abc?xyz+pqr{3}ab{2,}xy{4,5}pq{0,6}AB{0,}zz"
    test_eq(run1_match("a*abc?xyz+pqr{3}ab{2,}xy{4,5}pq{0,6}AB{0,}zz", "", "aaabcxyzpqrrrabbxyyyypqqqqAzz"), "aaabcxyzpqrrrabbxyyyypqqqqAzz"); // "a*abc?xyz+pqr{3}ab{2,}xy{4,5}pq{0,6}AB{0,}zz"
    test_eq(run1_match("a*abc?xyz+pqr{3}ab{2,}xy{4,5}pq{0,6}AB{0,}zz", "", "aaabcxyzpqrrrabbxyyyypqqqqqAzz"), "aaabcxyzpqrrrabbxyyyypqqqqqAzz"); // "a*abc?xyz+pqr{3}ab{2,}xy{4,5}pq{0,6}AB{0,}zz"
    test_eq(run1_match("a*abc?xyz+pqr{3}ab{2,}xy{4,5}pq{0,6}AB{0,}zz", "", "aaabcxyzpqrrrabbxyyyypqqqqqqAzz"), "aaabcxyzpqrrrabbxyyyypqqqqqqAzz"); // "a*abc?xyz+pqr{3}ab{2,}xy{4,5}pq{0,6}AB{0,}zz"
    test_eq(run1_match("a*abc?xyz+pqr{3}ab{2,}xy{4,5}pq{0,6}AB{0,}zz", "", "aaaabcxyzpqrrrabbxyyyypqAzz"), "aaaabcxyzpqrrrabbxyyyypqAzz"); // "a*abc?xyz+pqr{3}ab{2,}xy{4,5}pq{0,6}AB{0,}zz"
    test_eq(run1_match("a*abc?xyz+pqr{3}ab{2,}xy{4,5}pq{0,6}AB{0,}zz", "", "abxyzzpqrrrabbxyyyypqAzz"), "abxyzzpqrrrabbxyyyypqAzz"); // "a*abc?xyz+pqr{3}ab{2,}xy{4,5}pq{0,6}AB{0,}zz"
    test_eq(run1_match("a*abc?xyz+pqr{3}ab{2,}xy{4,5}pq{0,6}AB{0,}zz", "", "aabxyzzzpqrrrabbxyyyypqAzz"), "aabxyzzzpqrrrabbxyyyypqAzz"); // "a*abc?xyz+pqr{3}ab{2,}xy{4,5}pq{0,6}AB{0,}zz"
    test_eq(run1_match("a*abc?xyz+pqr{3}ab{2,}xy{4,5}pq{0,6}AB{0,}zz", "", "aaabxyzzzzpqrrrabbxyyyypqAzz"), "aaabxyzzzzpqrrrabbxyyyypqAzz"); // "a*abc?xyz+pqr{3}ab{2,}xy{4,5}pq{0,6}AB{0,}zz"
    test_eq(run1_match("a*abc?xyz+pqr{3}ab{2,}xy{4,5}pq{0,6}AB{0,}zz", "", "aaaabxyzzzzpqrrrabbxyyyypqAzz"), "aaaabxyzzzzpqrrrabbxyyyypqAzz"); // "a*abc?xyz+pqr{3}ab{2,}xy{4,5}pq{0,6}AB{0,}zz"
    test_eq(run1_match("a*abc?xyz+pqr{3}ab{2,}xy{4,5}pq{0,6}AB{0,}zz", "", "abcxyzzpqrrrabbxyyyypqAzz"), "abcxyzzpqrrrabbxyyyypqAzz"); // "a*abc?xyz+pqr{3}ab{2,}xy{4,5}pq{0,6}AB{0,}zz"
    test_eq(run1_match("a*abc?xyz+pqr{3}ab{2,}xy{4,5}pq{0,6}AB{0,}zz", "", "aabcxyzzzpqrrrabbxyyyypqAzz"), "aabcxyzzzpqrrrabbxyyyypqAzz"); // "a*abc?xyz+pqr{3}ab{2,}xy{4,5}pq{0,6}AB{0,}zz"
    test_eq(run1_match("a*abc?xyz+pqr{3}ab{2,}xy{4,5}pq{0,6}AB{0,}zz", "", "aaabcxyzzzzpqrrrabbxyyyypqAzz"), "aaabcxyzzzzpqrrrabbxyyyypqAzz"); // "a*abc?xyz+pqr{3}ab{2,}xy{4,5}pq{0,6}AB{0,}zz"
    test_eq(run1_match("a*abc?xyz+pqr{3}ab{2,}xy{4,5}pq{0,6}AB{0,}zz", "", "aaaabcxyzzzzpqrrrabbxyyyypqAzz"), "aaaabcxyzzzzpqrrrabbxyyyypqAzz"); // "a*abc?xyz+pqr{3}ab{2,}xy{4,5}pq{0,6}AB{0,}zz"
    test_eq(run1_match("a*abc?xyz+pqr{3}ab{2,}xy{4,5}pq{0,6}AB{0,}zz", "", "aaaabcxyzzzzpqrrrabbbxyyyypqAzz"), "aaaabcxyzzzzpqrrrabbbxyyyypqAzz"); // "a*abc?xyz+pqr{3}ab{2,}xy{4,5}pq{0,6}AB{0,}zz"
    test_eq(run1_match("a*abc?xyz+pqr{3}ab{2,}xy{4,5}pq{0,6}AB{0,}zz", "", "aaaabcxyzzzzpqrrrabbbxyyyyypqAzz"), "aaaabcxyzzzzpqrrrabbbxyyyyypqAzz"); // "a*abc?xyz+pqr{3}ab{2,}xy{4,5}pq{0,6}AB{0,}zz"
    test_eq(run1_match("a*abc?xyz+pqr{3}ab{2,}xy{4,5}pq{0,6}AB{0,}zz", "", "aaabcxyzpqrrrabbxyyyypABzz"), "aaabcxyzpqrrrabbxyyyypABzz"); // "a*abc?xyz+pqr{3}ab{2,}xy{4,5}pq{0,6}AB{0,}zz"
    test_eq(run1_match("a*abc?xyz+pqr{3}ab{2,}xy{4,5}pq{0,6}AB{0,}zz", "", "aaabcxyzpqrrrabbxyyyypABBzz"), "aaabcxyzpqrrrabbxyyyypABBzz"); // "a*abc?xyz+pqr{3}ab{2,}xy{4,5}pq{0,6}AB{0,}zz"
    test_eq(run1_match("a*abc?xyz+pqr{3}ab{2,}xy{4,5}pq{0,6}AB{0,}zz", "", ">>>aaabxyzpqrrrabbxyyyypqAzz"), "aaabxyzpqrrrabbxyyyypqAzz"); // "a*abc?xyz+pqr{3}ab{2,}xy{4,5}pq{0,6}AB{0,}zz"
    test_eq(run1_match("a*abc?xyz+pqr{3}ab{2,}xy{4,5}pq{0,6}AB{0,}zz", "", ">aaaabxyzpqrrrabbxyyyypqAzz"), "aaaabxyzpqrrrabbxyyyypqAzz"); // "a*abc?xyz+pqr{3}ab{2,}xy{4,5}pq{0,6}AB{0,}zz"
    test_eq(run1_match("a*abc?xyz+pqr{3}ab{2,}xy{4,5}pq{0,6}AB{0,}zz", "", ">>>>abcxyzpqrrrabbxyyyypqAzz"), "abcxyzpqrrrabbxyyyypqAzz"); // "a*abc?xyz+pqr{3}ab{2,}xy{4,5}pq{0,6}AB{0,}zz"
    test_eq(run1_match("^(abc){1,2}zz", "", "abczz"), "abczz,abc");
    test_eq(run1_match("^(abc){1,2}zz", "", "abcabczz"), "abcabczz,abc");
    test_eq(run1_match("^(b+?|a){1,2}?c", "", "bc"), "bc,b");
    test_eq(run1_match("^(b+?|a){1,2}?c", "", "bbc"), "bbc,b");
    test_eq(run1_match("^(b+?|a){1,2}?c", "", "bbbc"), "bbbc,bb");
    test_eq(run1_match("^(b+?|a){1,2}?c", "", "bac"), "bac,a");
    test_eq(run1_match("^(b+?|a){1,2}?c", "", "bbac"), "bbac,a");
    test_eq(run1_match("^(b+?|a){1,2}?c", "", "aac"), "aac,a");
    test_eq(run1_match("^(b+?|a){1,2}?c", "", "abbbbbbbbbbbc"), "abbbbbbbbbbbc,bbbbbbbbbbb");
    test_eq(run1_match("^(b+?|a){1,2}?c", "", "bbbbbbbbbbbac"), "bbbbbbbbbbbac,a");
    test_eq(run1_match("^(b+|a){1,2}c", "", "bc"), "bc,b");
    test_eq(run1_match("^(b+|a){1,2}c", "", "bbc"), "bbc,bb");
    test_eq(run1_match("^(b+|a){1,2}c", "", "bbbc"), "bbbc,bbb");
    test_eq(run1_match("^(b+|a){1,2}c", "", "bac"), "bac,a");
    test_eq(run1_match("^(b+|a){1,2}c", "", "bbac"), "bbac,a");
    test_eq(run1_match("^(b+|a){1,2}c", "", "aac"), "aac,a");
    test_eq(run1_match("^(b+|a){1,2}c", "", "abbbbbbbbbbbc"), "abbbbbbbbbbbc,bbbbbbbbbbb");
    test_eq(run1_match("^(b+|a){1,2}c", "", "bbbbbbbbbbbac"), "bbbbbbbbbbbac,a");
    test_eq(run1_match("^(b+|a){1,2}c", "", "bbc"), "bbc,bb");
    test_eq(run1_match("^(b*|ba){1,2}?bc", "", "babc"), "babc,ba");
    test_eq(run1_match("^(b*|ba){1,2}?bc", "", "bbabc"), "bbabc,ba");
    test_eq(run1_match("^(b*|ba){1,2}?bc", "", "bababc"), "bababc,ba");
    test_eq(run1_match("^(ba|b*){1,2}?bc", "", "babc"), "babc,ba");
    test_eq(run1_match("^(ba|b*){1,2}?bc", "", "bbabc"), "bbabc,ba");
    test_eq(run1_match("^(ba|b*){1,2}?bc", "", "bababc"), "bababc,ba");
    test_eq(run1_match("^[ab\\]cde]", "", "athing"), "a");
    test_eq(run1_match("^[ab\\]cde]", "", "bthing"), "b");
    test_eq(run1_match("^[ab\\]cde]", "", "]thing"), "]");
    test_eq(run1_match("^[ab\\]cde]", "", "cthing"), "c");
    test_eq(run1_match("^[ab\\]cde]", "", "dthing"), "d");
    test_eq(run1_match("^[ab\\]cde]", "", "ething"), "e");
    test_eq(run1_match("^[^ab\\]cde]", "", "fthing"), "f");
    test_eq(run1_match("^[^ab\\]cde]", "", "[thing"), "[");
    test_eq(run1_match("^[^ab\\]cde]", "", "\\thing"), "\\");
    test_eq(run1_match("^[^ab\\]cde]", "", "*** Failers"), "*");
    test_eq(run1_match("^[0-9]+$", "", "0"), "0");
    test_eq(run1_match("^[0-9]+$", "", "1"), "1");
    test_eq(run1_match("^[0-9]+$", "", "2"), "2");
    test_eq(run1_match("^[0-9]+$", "", "3"), "3");
    test_eq(run1_match("^[0-9]+$", "", "4"), "4");
    test_eq(run1_match("^[0-9]+$", "", "5"), "5");
    test_eq(run1_match("^[0-9]+$", "", "6"), "6");
    test_eq(run1_match("^[0-9]+$", "", "7"), "7");
    test_eq(run1_match("^[0-9]+$", "", "8"), "8");
    test_eq(run1_match("^[0-9]+$", "", "9"), "9");
    test_eq(run1_match("^[0-9]+$", "", "10"), "10");
    test_eq(run1_match("^[0-9]+$", "", "100"), "100");
    test_eq(run1_match("^.*nter", "", "enter"), "enter");
    test_eq(run1_match("^.*nter", "", "inter"), "inter");
    test_eq(run1_match("^.*nter", "", "uponter"), "uponter");
    test_eq(run1_match("^xxx[0-9]+$", "", "xxx0"), "xxx0"); // "^xxx[0-9]+$"
    test_eq(run1_match("^xxx[0-9]+$", "", "xxx1234"), "xxx1234"); // "^xxx[0-9]+$"
    test_eq(run1_match("^.+[0-9][0-9][0-9]$", "", "x123"), "x123");
    test_eq(run1_match("^.+[0-9][0-9][0-9]$", "", "xx123"), "xx123");
    test_eq(run1_match("^.+[0-9][0-9][0-9]$", "", "123456"), "123456");
    test_eq(run1_match("^.+[0-9][0-9][0-9]$", "", "x1234"), "x1234");
    test_eq(run1_match("^.+?[0-9][0-9][0-9]$", "", "x123"), "x123");
    test_eq(run1_match("^.+?[0-9][0-9][0-9]$", "", "xx123"), "xx123");
    test_eq(run1_match("^.+?[0-9][0-9][0-9]$", "", "123456"), "123456");
    test_eq(run1_match("^.+?[0-9][0-9][0-9]$", "", "x1234"), "x1234");
    test_eq(run1_match("^([^!]+)!(.+)=apquxz\\.ixr\\.zzz\\.ac\\.uk$", "", "abc!pqr=apquxz.ixr.zzz.ac.uk"), "abc!pqr=apquxz.ixr.zzz.ac.uk,abc,pqr"); // "^([^!]+)!(.+)=apquxz\\.ixr\\.zzz\\.ac\\.uk$"
    test_eq(run1_match(":", "", "Well, we need a colon: somewhere"), ":");
    test_eq(run1_match("([\\da-f:]+)$", "i", "0abc"), "0abc,0abc");
    test_eq(run1_match("([\\da-f:]+)$", "i", "abc"), "abc,abc");
    test_eq(run1_match("([\\da-f:]+)$", "i", "fed"), "fed,fed");
    test_eq(run1_match("([\\da-f:]+)$", "i", "E"), "E,E");
    test_eq(run1_match("([\\da-f:]+)$", "i", "::"), "::,::");
    test_eq(run1_match("([\\da-f:]+)$", "i", "5f03:12C0::932e"), "5f03:12C0::932e,5f03:12C0::932e");
    test_eq(run1_match("([\\da-f:]+)$", "i", "fed def"), "def,def");
    test_eq(run1_match("([\\da-f:]+)$", "i", "Any old stuff"), "ff,ff");
    test_eq(run1_match("^.*\\.(\\d{1,3})\\.(\\d{1,3})\\.(\\d{1,3})$", "", ".1.2.3"), ".1.2.3,1,2,3");
    test_eq(run1_match("^.*\\.(\\d{1,3})\\.(\\d{1,3})\\.(\\d{1,3})$", "", "A.12.123.0"), "A.12.123.0,12,123,0");
    test_eq(run1_match("^(\\d+)\\s+IN\\s+SOA\\s+(\\S+)\\s+(\\S+)\\s*\\(\\s*$", "", "1 IN SOA non-sp1 non-sp2("), "1 IN SOA non-sp1 non-sp2(,1,non-sp1,non-sp2");
    test_eq(run1_match("^(\\d+)\\s+IN\\s+SOA\\s+(\\S+)\\s+(\\S+)\\s*\\(\\s*$", "", "1    IN    SOA    non-sp1    non-sp2   ("), "1    IN    SOA    non-sp1    non-sp2   (,1,non-sp1,non-sp2");
    test_eq(run1_match("^[a-zA-Z\\d][a-zA-Z\\d\\-]*(\\.[a-zA-Z\\d][a-zA-Z\\d\\-]*)*\\.$", "", "a."), "a.,");
    test_eq(run1_match("^[a-zA-Z\\d][a-zA-Z\\d\\-]*(\\.[a-zA-Z\\d][a-zA-Z\\d\\-]*)*\\.$", "", "Z."), "Z.,");
    test_eq(run1_match("^[a-zA-Z\\d][a-zA-Z\\d\\-]*(\\.[a-zA-Z\\d][a-zA-Z\\d\\-]*)*\\.$", "", "2."), "2.,");
    test_eq(run1_match("^[a-zA-Z\\d][a-zA-Z\\d\\-]*(\\.[a-zA-Z\\d][a-zA-Z\\d\\-]*)*\\.$", "", "ab-c.pq-r."), "ab-c.pq-r.,.pq-r");
    test_eq(run1_match("^[a-zA-Z\\d][a-zA-Z\\d\\-]*(\\.[a-zA-Z\\d][a-zA-Z\\d\\-]*)*\\.$", "", "sxk.zzz.ac.uk."), "sxk.zzz.ac.uk.,.uk");
    test_eq(run1_match("^[a-zA-Z\\d][a-zA-Z\\d\\-]*(\\.[a-zA-Z\\d][a-zA-Z\\d\\-]*)*\\.$", "", "x-.y-."), "x-.y-.,.y-");
    test_eq(run1_match("^\\*\\.[a-z]([a-z\\-\\d]*[a-z\\d]+)?(\\.[a-z]([a-z\\-\\d]*[a-z\\d]+)?)*$", "", "*.a"), "*.a,,,");
    test_eq(run1_match("^\\*\\.[a-z]([a-z\\-\\d]*[a-z\\d]+)?(\\.[a-z]([a-z\\-\\d]*[a-z\\d]+)?)*$", "", "*.b0-a"), "*.b0-a,0-a,,");
    test_eq(run1_match("^\\*\\.[a-z]([a-z\\-\\d]*[a-z\\d]+)?(\\.[a-z]([a-z\\-\\d]*[a-z\\d]+)?)*$", "", "*.c3-b.c"), "*.c3-b.c,3-b,.c,");
    test_eq(run1_match("^\\*\\.[a-z]([a-z\\-\\d]*[a-z\\d]+)?(\\.[a-z]([a-z\\-\\d]*[a-z\\d]+)?)*$", "", "*.c-a.b-c"), "*.c-a.b-c,-a,.b-c,-c");
    test_eq(run1_match("^(?=ab(de))(abd)(e)", "", "abde"), "abde,de,abd,e");
    test_eq(run1_match("^(?!(ab)de|x)(abd)(f)", "", "abdf"), "abdf,,abd,f");
    test_eq(run1_match("^(?=(ab(cd)))(ab)", "", "abcd"), "ab,abcd,cd,ab");
    test_eq(run1_match("^[\\da-f](\\.[\\da-f])*$", "i", "a.b.c.d"), "a.b.c.d,.d");
    test_eq(run1_match("^[\\da-f](\\.[\\da-f])*$", "i", "A.B.C.D"), "A.B.C.D,.D");
    test_eq(run1_match("^[\\da-f](\\.[\\da-f])*$", "i", "a.b.c.1.2.3.C"), "a.b.c.1.2.3.C,.C"); // "^[\\da-f](\\.[\\da-f])*$"
    // Skipping Unicode-unfriendly ^\".*\"\s*(;.*)?$
    // Skipping Unicode-unfriendly ^\".*\"\s*(;.*)?$
    // Skipping Unicode-unfriendly ^\".*\"\s*(;.*)?$
    test_eq(run1_match("^ab\\sc$", "", "ab c"), "ab c");
    test_eq(run1_match("^ab\\sc$", "", "ab c"), "ab c"); // "^ab\\sc$"
    // Skipping Unicode-unfriendly ^a\ b[c]d$
    test_eq(run1_match("^(a(b(c)))(d(e(f)))(h(i(j)))(k(l(m)))$", "", "abcdefhijklm"), "abcdefhijklm,abc,bc,c,def,ef,f,hij,ij,j,klm,lm,m");
    test_eq(run1_match("^(?:a(b(c)))(?:d(e(f)))(?:h(i(j)))(?:k(l(m)))$", "", "abcdefhijklm"), "abcdefhijklm,bc,c,ef,f,ij,j,lm,m");
    test_eq(run1_match("^a*\\w", "", "z"), "z");
    test_eq(run1_match("^a*\\w", "", "az"), "az");
    test_eq(run1_match("^a*\\w", "", "aaaz"), "aaaz");
    test_eq(run1_match("^a*\\w", "", "a"), "a");
    test_eq(run1_match("^a*\\w", "", "aa"), "aa");
    test_eq(run1_match("^a*\\w", "", "aaaa"), "aaaa");
    test_eq(run1_match("^a*\\w", "", "a+"), "a");
    test_eq(run1_match("^a*\\w", "", "aa+"), "aa");
    test_eq(run1_match("^a*?\\w", "", "z"), "z");
    test_eq(run1_match("^a*?\\w", "", "az"), "a");
    test_eq(run1_match("^a*?\\w", "", "aaaz"), "a");
    test_eq(run1_match("^a*?\\w", "", "a"), "a");
    test_eq(run1_match("^a*?\\w", "", "aa"), "a");
    test_eq(run1_match("^a*?\\w", "", "aaaa"), "a");
    test_eq(run1_match("^a*?\\w", "", "a+"), "a");
    test_eq(run1_match("^a*?\\w", "", "aa+"), "a");
    test_eq(run1_match("^a+\\w", "", "az"), "az");
    test_eq(run1_match("^a+\\w", "", "aaaz"), "aaaz");
    test_eq(run1_match("^a+\\w", "", "aa"), "aa");
    test_eq(run1_match("^a+\\w", "", "aaaa"), "aaaa");
    test_eq(run1_match("^a+\\w", "", "aa+"), "aa");
    test_eq(run1_match("^a+?\\w", "", "az"), "az");
    test_eq(run1_match("^a+?\\w", "", "aaaz"), "aa");
    test_eq(run1_match("^a+?\\w", "", "aa"), "aa");
    test_eq(run1_match("^a+?\\w", "", "aaaa"), "aa");
    test_eq(run1_match("^a+?\\w", "", "aa+"), "aa");
    test_eq(run1_match("^\\d{8}\\w{2,}", "", "1234567890"), "1234567890");
    test_eq(run1_match("^\\d{8}\\w{2,}", "", "12345678ab"), "12345678ab");
    test_eq(run1_match("^\\d{8}\\w{2,}", "", "12345678__"), "12345678__");
    test_eq(run1_match("^[aeiou\\d]{4,5}$", "", "uoie"), "uoie");
    test_eq(run1_match("^[aeiou\\d]{4,5}$", "", "1234"), "1234");
    test_eq(run1_match("^[aeiou\\d]{4,5}$", "", "12345"), "12345");
    test_eq(run1_match("^[aeiou\\d]{4,5}$", "", "aaaaa"), "aaaaa");
    test_eq(run1_match("^[aeiou\\d]{4,5}?", "", "uoie"), "uoie");
    test_eq(run1_match("^[aeiou\\d]{4,5}?", "", "1234"), "1234");
    test_eq(run1_match("^[aeiou\\d]{4,5}?", "", "12345"), "1234");
    test_eq(run1_match("^[aeiou\\d]{4,5}?", "", "aaaaa"), "aaaa");
    test_eq(run1_match("^[aeiou\\d]{4,5}?", "", "123456"), "1234");
    test_eq(run1_match("^From +([^ ]+) +[a-zA-Z][a-zA-Z][a-zA-Z] +[a-zA-Z][a-zA-Z][a-zA-Z] +[0-9]?[0-9] +[0-9][0-9]:[0-9][0-9]", "", "From abcd  Mon Sep 01 12:33:02 1997"), "From abcd  Mon Sep 01 12:33,abcd");
    test_eq(run1_match("^From\\s+\\S+\\s+([a-zA-Z]{3}\\s+){2}\\d{1,2}\\s+\\d\\d:\\d\\d", "", "From abcd  Mon Sep 01 12:33:02 1997"), "From abcd  Mon Sep 01 12:33,Sep ");
    test_eq(run1_match("^From\\s+\\S+\\s+([a-zA-Z]{3}\\s+){2}\\d{1,2}\\s+\\d\\d:\\d\\d", "", "From abcd  Mon Sep  1 12:33:02 1997"), "From abcd  Mon Sep  1 12:33,Sep  ");
    test_eq(run1_match("\\w+(?=\\t)", "", "the quick brown\u{9} fox"), "brown");
    test_eq(run1_match("foo(?!bar)(.*)", "", "foobar is foolish see?"), "foolish see?,lish see?"); // "foo(?!bar)(.*)"
    test_eq(run1_match("(?:(?!foo)...|^.{0,2})bar(.*)", "", "foobar crowbar etc"), "rowbar etc, etc"); // "(?:(?!foo)...|^.{0,2})bar(.*)"
    test_eq(run1_match("(?:(?!foo)...|^.{0,2})bar(.*)", "", "barrel"), "barrel,rel"); // "(?:(?!foo)...|^.{0,2})bar(.*)"
    test_eq(run1_match("(?:(?!foo)...|^.{0,2})bar(.*)", "", "2barrel"), "2barrel,rel"); // "(?:(?!foo)...|^.{0,2})bar(.*)"
    test_eq(run1_match("(?:(?!foo)...|^.{0,2})bar(.*)", "", "A barrel"), "A barrel,rel"); // "(?:(?!foo)...|^.{0,2})bar(.*)"
    test_eq(run1_match("^(\\D*)(?=\\d)(?!123)", "", "abc456"), "abc,abc");
    test_eq(run1_match("^1234", "", "1234"), "1234");
    test_eq(run1_match("^1234", "", "1234"), "1234");
    test_eq(run1_match("abcd", "", "abcd"), "abcd");
    test_eq(run1_match("^abcd", "", "abcd"), "abcd");
    test_eq(run1_match("(?!^)abc", "", "the abc"), "abc");
    test_eq(run1_match("(?=^)abc", "", "abc"), "abc");
    test_eq(run1_match("^[ab]{1,3}(ab*|b)", "", "aabbbbb"), "aabb,b");
    test_eq(run1_match("^[ab]{1,3}?(ab*|b)", "", "aabbbbb"), "aabbbbb,abbbbb");
    test_eq(run1_match("^[ab]{1,3}?(ab*?|b)", "", "aabbbbb"), "aa,a");
    test_eq(run1_match("^[ab]{1,3}(ab*?|b)", "", "aabbbbb"), "aabb,b"); // "^[ab]{1,3}(ab*?|b)"
    // Skipping Unicode-unfriendly (?:[\040\t]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff])*\))*\))*(?:(?:[^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff]+(?![^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff])|"(?:[^\\\x80-\xff\n\015"]|\\[^\x80-\xff])*")(?:(?:[\040\t]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff])*\))*\))*\.(?:[\040\t]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff])*\))*\))*(?:[^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff]+(?![^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff])|"(?:[^\\\x80-\xff\n\015"]|\\[^\x80-\xff])*"))*(?:[\040\t]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff])*\))*\))*@(?:[\040\t]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff])*\))*\))*(?:[^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff]+(?![^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff])|\[(?:[^\\\x80-\xff\n\015\[\]]|\\[^\x80-\xff])*\])(?:(?:[\040\t]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff])*\))*\))*\.(?:[\040\t]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff])*\))*\))*(?:[^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff]+(?![^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff])|\[(?:[^\\\x80-\xff\n\015\[\]]|\\[^\x80-\xff])*\]))*|(?:[^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff]+(?![^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff])|"(?:[^\\\x80-\xff\n\015"]|\\[^\x80-\xff])*")(?:[^()<>@,;:".\\\[\]\x80-\xff\000-\010\012-\037]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff])*\))*\)|"(?:[^\\\x80-\xff\n\015"]|\\[^\x80-\xff])*")*<(?:[\040\t]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff])*\))*\))*(?:@(?:[\040\t]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff])*\))*\))*(?:[^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff]+(?![^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff])|\[(?:[^\\\x80-\xff\n\015\[\]]|\\[^\x80-\xff])*\])(?:(?:[\040\t]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff])*\))*\))*\.(?:[\040\t]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff])*\))*\))*(?:[^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff]+(?![^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff])|\[(?:[^\\\x80-\xff\n\015\[\]]|\\[^\x80-\xff])*\]))*(?:(?:[\040\t]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff])*\))*\))*,(?:[\040\t]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff])*\))*\))*@(?:[\040\t]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff])*\))*\))*(?:[^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff]+(?![^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff])|\[(?:[^\\\x80-\xff\n\015\[\]]|\\[^\x80-\xff])*\])(?:(?:[\040\t]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff])*\))*\))*\.(?:[\040\t]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff])*\))*\))*(?:[^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff]+(?![^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff])|\[(?:[^\\\x80-\xff\n\015\[\]]|\\[^\x80-\xff])*\]))*)*:(?:[\040\t]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff])*\))*\))*)?(?:[^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff]+(?![^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff])|"(?:[^\\\x80-\xff\n\015"]|\\[^\x80-\xff])*")(?:(?:[\040\t]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff])*\))*\))*\.(?:[\040\t]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff])*\))*\))*(?:[^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff]+(?![^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff])|"(?:[^\\\x80-\xff\n\015"]|\\[^\x80-\xff])*"))*(?:[\040\t]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff])*\))*\))*@(?:[\040\t]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff])*\))*\))*(?:[^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff]+(?![^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff])|\[(?:[^\\\x80-\xff\n\015\[\]]|\\[^\x80-\xff])*\])(?:(?:[\040\t]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff])*\))*\))*\.(?:[\040\t]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff])*\))*\))*(?:[^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff]+(?![^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff])|\[(?:[^\\\x80-\xff\n\015\[\]]|\\[^\x80-\xff])*\]))*(?:[\040\t]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff])*\))*\))*>)(?:[\040\t]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff])*\))*\))*
    // Skipping Unicode-unfriendly (?:[\040\t]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff])*\))*\))*(?:(?:[^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff]+(?![^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff])|"(?:[^\\\x80-\xff\n\015"]|\\[^\x80-\xff])*")(?:(?:[\040\t]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff])*\))*\))*\.(?:[\040\t]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff])*\))*\))*(?:[^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff]+(?![^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff])|"(?:[^\\\x80-\xff\n\015"]|\\[^\x80-\xff])*"))*(?:[\040\t]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff])*\))*\))*@(?:[\040\t]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff])*\))*\))*(?:[^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff]+(?![^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff])|\[(?:[^\\\x80-\xff\n\015\[\]]|\\[^\x80-\xff])*\])(?:(?:[\040\t]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff])*\))*\))*\.(?:[\040\t]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff])*\))*\))*(?:[^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff]+(?![^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff])|\[(?:[^\\\x80-\xff\n\015\[\]]|\\[^\x80-\xff])*\]))*|(?:[^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff]+(?![^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff])|"(?:[^\\\x80-\xff\n\015"]|\\[^\x80-\xff])*")(?:[^()<>@,;:".\\\[\]\x80-\xff\000-\010\012-\037]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff])*\))*\)|"(?:[^\\\x80-\xff\n\015"]|\\[^\x80-\xff])*")*<(?:[\040\t]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff])*\))*\))*(?:@(?:[\040\t]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff])*\))*\))*(?:[^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff]+(?![^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff])|\[(?:[^\\\x80-\xff\n\015\[\]]|\\[^\x80-\xff])*\])(?:(?:[\040\t]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff])*\))*\))*\.(?:[\040\t]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff])*\))*\))*(?:[^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff]+(?![^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff])|\[(?:[^\\\x80-\xff\n\015\[\]]|\\[^\x80-\xff])*\]))*(?:(?:[\040\t]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff])*\))*\))*,(?:[\040\t]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff])*\))*\))*@(?:[\040\t]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff])*\))*\))*(?:[^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff]+(?![^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff])|\[(?:[^\\\x80-\xff\n\015\[\]]|\\[^\x80-\xff])*\])(?:(?:[\040\t]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff])*\))*\))*\.(?:[\040\t]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff])*\))*\))*(?:[^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff]+(?![^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff])|\[(?:[^\\\x80-\xff\n\015\[\]]|\\[^\x80-\xff])*\]))*)*:(?:[\040\t]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff])*\))*\))*)?(?:[^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff]+(?![^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff])|"(?:[^\\\x80-\xff\n\015"]|\\[^\x80-\xff])*")(?:(?:[\040\t]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff])*\))*\))*\.(?:[\040\t]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff])*\))*\))*(?:[^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff]+(?![^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff])|"(?:[^\\\x80-\xff\n\015"]|\\[^\x80-\xff])*"))*(?:[\040\t]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff])*\))*\))*@(?:[\040\t]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff])*\))*\))*(?:[^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff]+(?![^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff])|\[(?:[^\\\x80-\xff\n\015\[\]]|\\[^\x80-\xff])*\])(?:(?:[\040\t]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff])*\))*\))*\.(?:[\040\t]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff])*\))*\))*(?:[^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff]+(?![^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff])|\[(?:[^\\\x80-\xff\n\015\[\]]|\\[^\x80-\xff])*\]))*(?:[\040\t]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff])*\))*\))*>)(?:[\040\t]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff])*\))*\))*
    // Skipping Unicode-unfriendly (?:[\040\t]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff])*\))*\))*(?:(?:[^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff]+(?![^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff])|"(?:[^\\\x80-\xff\n\015"]|\\[^\x80-\xff])*")(?:(?:[\040\t]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff])*\))*\))*\.(?:[\040\t]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff])*\))*\))*(?:[^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff]+(?![^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff])|"(?:[^\\\x80-\xff\n\015"]|\\[^\x80-\xff])*"))*(?:[\040\t]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff])*\))*\))*@(?:[\040\t]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff])*\))*\))*(?:[^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff]+(?![^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff])|\[(?:[^\\\x80-\xff\n\015\[\]]|\\[^\x80-\xff])*\])(?:(?:[\040\t]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff])*\))*\))*\.(?:[\040\t]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff])*\))*\))*(?:[^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff]+(?![^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff])|\[(?:[^\\\x80-\xff\n\015\[\]]|\\[^\x80-\xff])*\]))*|(?:[^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff]+(?![^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff])|"(?:[^\\\x80-\xff\n\015"]|\\[^\x80-\xff])*")(?:[^()<>@,;:".\\\[\]\x80-\xff\000-\010\012-\037]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff])*\))*\)|"(?:[^\\\x80-\xff\n\015"]|\\[^\x80-\xff])*")*<(?:[\040\t]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff])*\))*\))*(?:@(?:[\040\t]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff])*\))*\))*(?:[^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff]+(?![^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff])|\[(?:[^\\\x80-\xff\n\015\[\]]|\\[^\x80-\xff])*\])(?:(?:[\040\t]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff])*\))*\))*\.(?:[\040\t]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff])*\))*\))*(?:[^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff]+(?![^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff])|\[(?:[^\\\x80-\xff\n\015\[\]]|\\[^\x80-\xff])*\]))*(?:(?:[\040\t]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff])*\))*\))*,(?:[\040\t]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff])*\))*\))*@(?:[\040\t]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff])*\))*\))*(?:[^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff]+(?![^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff])|\[(?:[^\\\x80-\xff\n\015\[\]]|\\[^\x80-\xff])*\])(?:(?:[\040\t]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff])*\))*\))*\.(?:[\040\t]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff])*\))*\))*(?:[^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff]+(?![^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff])|\[(?:[^\\\x80-\xff\n\015\[\]]|\\[^\x80-\xff])*\]))*)*:(?:[\040\t]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff])*\))*\))*)?(?:[^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff]+(?![^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff])|"(?:[^\\\x80-\xff\n\015"]|\\[^\x80-\xff])*")(?:(?:[\040\t]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff])*\))*\))*\.(?:[\040\t]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff])*\))*\))*(?:[^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff]+(?![^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff])|"(?:[^\\\x80-\xff\n\015"]|\\[^\x80-\xff])*"))*(?:[\040\t]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff])*\))*\))*@(?:[\040\t]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff])*\))*\))*(?:[^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff]+(?![^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff])|\[(?:[^\\\x80-\xff\n\015\[\]]|\\[^\x80-\xff])*\])(?:(?:[\040\t]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff])*\))*\))*\.(?:[\040\t]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff])*\))*\))*(?:[^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff]+(?![^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff])|\[(?:[^\\\x80-\xff\n\015\[\]]|\\[^\x80-\xff])*\]))*(?:[\040\t]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff])*\))*\))*>)(?:[\040\t]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff])*\))*\))*
    // Skipping Unicode-unfriendly (?:[\040\t]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff])*\))*\))*(?:(?:[^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff]+(?![^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff])|"(?:[^\\\x80-\xff\n\015"]|\\[^\x80-\xff])*")(?:(?:[\040\t]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff])*\))*\))*\.(?:[\040\t]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff])*\))*\))*(?:[^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff]+(?![^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff])|"(?:[^\\\x80-\xff\n\015"]|\\[^\x80-\xff])*"))*(?:[\040\t]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff])*\))*\))*@(?:[\040\t]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff])*\))*\))*(?:[^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff]+(?![^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff])|\[(?:[^\\\x80-\xff\n\015\[\]]|\\[^\x80-\xff])*\])(?:(?:[\040\t]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff])*\))*\))*\.(?:[\040\t]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff])*\))*\))*(?:[^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff]+(?![^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff])|\[(?:[^\\\x80-\xff\n\015\[\]]|\\[^\x80-\xff])*\]))*|(?:[^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff]+(?![^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff])|"(?:[^\\\x80-\xff\n\015"]|\\[^\x80-\xff])*")(?:[^()<>@,;:".\\\[\]\x80-\xff\000-\010\012-\037]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff])*\))*\)|"(?:[^\\\x80-\xff\n\015"]|\\[^\x80-\xff])*")*<(?:[\040\t]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff])*\))*\))*(?:@(?:[\040\t]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff])*\))*\))*(?:[^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff]+(?![^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff])|\[(?:[^\\\x80-\xff\n\015\[\]]|\\[^\x80-\xff])*\])(?:(?:[\040\t]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff])*\))*\))*\.(?:[\040\t]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff])*\))*\))*(?:[^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff]+(?![^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff])|\[(?:[^\\\x80-\xff\n\015\[\]]|\\[^\x80-\xff])*\]))*(?:(?:[\040\t]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff])*\))*\))*,(?:[\040\t]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff])*\))*\))*@(?:[\040\t]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff])*\))*\))*(?:[^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff]+(?![^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff])|\[(?:[^\\\x80-\xff\n\015\[\]]|\\[^\x80-\xff])*\])(?:(?:[\040\t]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff])*\))*\))*\.(?:[\040\t]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff])*\))*\))*(?:[^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff]+(?![^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff])|\[(?:[^\\\x80-\xff\n\015\[\]]|\\[^\x80-\xff])*\]))*)*:(?:[\040\t]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff])*\))*\))*)?(?:[^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff]+(?![^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff])|"(?:[^\\\x80-\xff\n\015"]|\\[^\x80-\xff])*")(?:(?:[\040\t]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff])*\))*\))*\.(?:[\040\t]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff])*\))*\))*(?:[^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff]+(?![^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff])|"(?:[^\\\x80-\xff\n\015"]|\\[^\x80-\xff])*"))*(?:[\040\t]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff])*\))*\))*@(?:[\040\t]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff])*\))*\))*(?:[^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff]+(?![^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff])|\[(?:[^\\\x80-\xff\n\015\[\]]|\\[^\x80-\xff])*\])(?:(?:[\040\t]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff])*\))*\))*\.(?:[\040\t]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff])*\))*\))*(?:[^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff]+(?![^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff])|\[(?:[^\\\x80-\xff\n\015\[\]]|\\[^\x80-\xff])*\]))*(?:[\040\t]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff])*\))*\))*>)(?:[\040\t]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff])*\))*\))*
    // Skipping Unicode-unfriendly (?:[\040\t]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff])*\))*\))*(?:(?:[^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff]+(?![^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff])|"(?:[^\\\x80-\xff\n\015"]|\\[^\x80-\xff])*")(?:(?:[\040\t]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff])*\))*\))*\.(?:[\040\t]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff])*\))*\))*(?:[^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff]+(?![^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff])|"(?:[^\\\x80-\xff\n\015"]|\\[^\x80-\xff])*"))*(?:[\040\t]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff])*\))*\))*@(?:[\040\t]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff])*\))*\))*(?:[^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff]+(?![^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff])|\[(?:[^\\\x80-\xff\n\015\[\]]|\\[^\x80-\xff])*\])(?:(?:[\040\t]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff])*\))*\))*\.(?:[\040\t]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff])*\))*\))*(?:[^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff]+(?![^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff])|\[(?:[^\\\x80-\xff\n\015\[\]]|\\[^\x80-\xff])*\]))*|(?:[^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff]+(?![^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff])|"(?:[^\\\x80-\xff\n\015"]|\\[^\x80-\xff])*")(?:[^()<>@,;:".\\\[\]\x80-\xff\000-\010\012-\037]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff])*\))*\)|"(?:[^\\\x80-\xff\n\015"]|\\[^\x80-\xff])*")*<(?:[\040\t]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff])*\))*\))*(?:@(?:[\040\t]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff])*\))*\))*(?:[^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff]+(?![^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff])|\[(?:[^\\\x80-\xff\n\015\[\]]|\\[^\x80-\xff])*\])(?:(?:[\040\t]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff])*\))*\))*\.(?:[\040\t]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff])*\))*\))*(?:[^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff]+(?![^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff])|\[(?:[^\\\x80-\xff\n\015\[\]]|\\[^\x80-\xff])*\]))*(?:(?:[\040\t]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff])*\))*\))*,(?:[\040\t]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff])*\))*\))*@(?:[\040\t]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff])*\))*\))*(?:[^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff]+(?![^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff])|\[(?:[^\\\x80-\xff\n\015\[\]]|\\[^\x80-\xff])*\])(?:(?:[\040\t]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff])*\))*\))*\.(?:[\040\t]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff])*\))*\))*(?:[^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff]+(?![^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff])|\[(?:[^\\\x80-\xff\n\015\[\]]|\\[^\x80-\xff])*\]))*)*:(?:[\040\t]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff])*\))*\))*)?(?:[^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff]+(?![^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff])|"(?:[^\\\x80-\xff\n\015"]|\\[^\x80-\xff])*")(?:(?:[\040\t]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff])*\))*\))*\.(?:[\040\t]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff])*\))*\))*(?:[^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff]+(?![^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff])|"(?:[^\\\x80-\xff\n\015"]|\\[^\x80-\xff])*"))*(?:[\040\t]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff])*\))*\))*@(?:[\040\t]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff])*\))*\))*(?:[^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff]+(?![^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff])|\[(?:[^\\\x80-\xff\n\015\[\]]|\\[^\x80-\xff])*\])(?:(?:[\040\t]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff])*\))*\))*\.(?:[\040\t]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff])*\))*\))*(?:[^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff]+(?![^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff])|\[(?:[^\\\x80-\xff\n\015\[\]]|\\[^\x80-\xff])*\]))*(?:[\040\t]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff])*\))*\))*>)(?:[\040\t]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff])*\))*\))*
    // Skipping Unicode-unfriendly (?:[\040\t]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff])*\))*\))*(?:(?:[^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff]+(?![^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff])|"(?:[^\\\x80-\xff\n\015"]|\\[^\x80-\xff])*")(?:(?:[\040\t]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff])*\))*\))*\.(?:[\040\t]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff])*\))*\))*(?:[^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff]+(?![^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff])|"(?:[^\\\x80-\xff\n\015"]|\\[^\x80-\xff])*"))*(?:[\040\t]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff])*\))*\))*@(?:[\040\t]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff])*\))*\))*(?:[^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff]+(?![^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff])|\[(?:[^\\\x80-\xff\n\015\[\]]|\\[^\x80-\xff])*\])(?:(?:[\040\t]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff])*\))*\))*\.(?:[\040\t]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff])*\))*\))*(?:[^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff]+(?![^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff])|\[(?:[^\\\x80-\xff\n\015\[\]]|\\[^\x80-\xff])*\]))*|(?:[^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff]+(?![^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff])|"(?:[^\\\x80-\xff\n\015"]|\\[^\x80-\xff])*")(?:[^()<>@,;:".\\\[\]\x80-\xff\000-\010\012-\037]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff])*\))*\)|"(?:[^\\\x80-\xff\n\015"]|\\[^\x80-\xff])*")*<(?:[\040\t]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff])*\))*\))*(?:@(?:[\040\t]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff])*\))*\))*(?:[^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff]+(?![^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff])|\[(?:[^\\\x80-\xff\n\015\[\]]|\\[^\x80-\xff])*\])(?:(?:[\040\t]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff])*\))*\))*\.(?:[\040\t]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff])*\))*\))*(?:[^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff]+(?![^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff])|\[(?:[^\\\x80-\xff\n\015\[\]]|\\[^\x80-\xff])*\]))*(?:(?:[\040\t]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff])*\))*\))*,(?:[\040\t]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff])*\))*\))*@(?:[\040\t]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff])*\))*\))*(?:[^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff]+(?![^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff])|\[(?:[^\\\x80-\xff\n\015\[\]]|\\[^\x80-\xff])*\])(?:(?:[\040\t]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff])*\))*\))*\.(?:[\040\t]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff])*\))*\))*(?:[^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff]+(?![^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff])|\[(?:[^\\\x80-\xff\n\015\[\]]|\\[^\x80-\xff])*\]))*)*:(?:[\040\t]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff])*\))*\))*)?(?:[^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff]+(?![^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff])|"(?:[^\\\x80-\xff\n\015"]|\\[^\x80-\xff])*")(?:(?:[\040\t]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff])*\))*\))*\.(?:[\040\t]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff])*\))*\))*(?:[^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff]+(?![^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff])|"(?:[^\\\x80-\xff\n\015"]|\\[^\x80-\xff])*"))*(?:[\040\t]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff])*\))*\))*@(?:[\040\t]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff])*\))*\))*(?:[^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff]+(?![^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff])|\[(?:[^\\\x80-\xff\n\015\[\]]|\\[^\x80-\xff])*\])(?:(?:[\040\t]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff])*\))*\))*\.(?:[\040\t]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff])*\))*\))*(?:[^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff]+(?![^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff])|\[(?:[^\\\x80-\xff\n\015\[\]]|\\[^\x80-\xff])*\]))*(?:[\040\t]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff])*\))*\))*>)(?:[\040\t]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff])*\))*\))*
    // Skipping Unicode-unfriendly (?:[\040\t]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff])*\))*\))*(?:(?:[^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff]+(?![^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff])|"(?:[^\\\x80-\xff\n\015"]|\\[^\x80-\xff])*")(?:(?:[\040\t]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff])*\))*\))*\.(?:[\040\t]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff])*\))*\))*(?:[^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff]+(?![^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff])|"(?:[^\\\x80-\xff\n\015"]|\\[^\x80-\xff])*"))*(?:[\040\t]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff])*\))*\))*@(?:[\040\t]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff])*\))*\))*(?:[^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff]+(?![^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff])|\[(?:[^\\\x80-\xff\n\015\[\]]|\\[^\x80-\xff])*\])(?:(?:[\040\t]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff])*\))*\))*\.(?:[\040\t]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff])*\))*\))*(?:[^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff]+(?![^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff])|\[(?:[^\\\x80-\xff\n\015\[\]]|\\[^\x80-\xff])*\]))*|(?:[^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff]+(?![^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff])|"(?:[^\\\x80-\xff\n\015"]|\\[^\x80-\xff])*")(?:[^()<>@,;:".\\\[\]\x80-\xff\000-\010\012-\037]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff])*\))*\)|"(?:[^\\\x80-\xff\n\015"]|\\[^\x80-\xff])*")*<(?:[\040\t]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff])*\))*\))*(?:@(?:[\040\t]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff])*\))*\))*(?:[^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff]+(?![^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff])|\[(?:[^\\\x80-\xff\n\015\[\]]|\\[^\x80-\xff])*\])(?:(?:[\040\t]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff])*\))*\))*\.(?:[\040\t]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff])*\))*\))*(?:[^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff]+(?![^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff])|\[(?:[^\\\x80-\xff\n\015\[\]]|\\[^\x80-\xff])*\]))*(?:(?:[\040\t]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff])*\))*\))*,(?:[\040\t]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff])*\))*\))*@(?:[\040\t]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff])*\))*\))*(?:[^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff]+(?![^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff])|\[(?:[^\\\x80-\xff\n\015\[\]]|\\[^\x80-\xff])*\])(?:(?:[\040\t]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff])*\))*\))*\.(?:[\040\t]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff])*\))*\))*(?:[^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff]+(?![^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff])|\[(?:[^\\\x80-\xff\n\015\[\]]|\\[^\x80-\xff])*\]))*)*:(?:[\040\t]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff])*\))*\))*)?(?:[^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff]+(?![^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff])|"(?:[^\\\x80-\xff\n\015"]|\\[^\x80-\xff])*")(?:(?:[\040\t]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff])*\))*\))*\.(?:[\040\t]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff])*\))*\))*(?:[^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff]+(?![^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff])|"(?:[^\\\x80-\xff\n\015"]|\\[^\x80-\xff])*"))*(?:[\040\t]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff])*\))*\))*@(?:[\040\t]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff])*\))*\))*(?:[^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff]+(?![^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff])|\[(?:[^\\\x80-\xff\n\015\[\]]|\\[^\x80-\xff])*\])(?:(?:[\040\t]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff])*\))*\))*\.(?:[\040\t]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff])*\))*\))*(?:[^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff]+(?![^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff])|\[(?:[^\\\x80-\xff\n\015\[\]]|\\[^\x80-\xff])*\]))*(?:[\040\t]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff])*\))*\))*>)(?:[\040\t]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff]|\((?:[^\\\x80-\xff\n\015()]|\\[^\x80-\xff])*\))*\))*
    // Skipping Unicode-unfriendly [\040\t]*(?:\([^\\\x80-\xff\n\015()]*(?:(?:\\[^\x80-\xff]|\([^\\\x80-\xff\n\015()]*(?:\\[^\x80-\xff][^\\\x80-\xff\n\015()]*)*\))[^\\\x80-\xff\n\015()]*)*\)[\040\t]*)*(?:(?:[^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff]+(?![^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff])|"[^\\\x80-\xff\n\015"]*(?:\\[^\x80-\xff][^\\\x80-\xff\n\015"]*)*")[\040\t]*(?:\([^\\\x80-\xff\n\015()]*(?:(?:\\[^\x80-\xff]|\([^\\\x80-\xff\n\015()]*(?:\\[^\x80-\xff][^\\\x80-\xff\n\015()]*)*\))[^\\\x80-\xff\n\015()]*)*\)[\040\t]*)*(?:\.[\040\t]*(?:\([^\\\x80-\xff\n\015()]*(?:(?:\\[^\x80-\xff]|\([^\\\x80-\xff\n\015()]*(?:\\[^\x80-\xff][^\\\x80-\xff\n\015()]*)*\))[^\\\x80-\xff\n\015()]*)*\)[\040\t]*)*(?:[^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff]+(?![^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff])|"[^\\\x80-\xff\n\015"]*(?:\\[^\x80-\xff][^\\\x80-\xff\n\015"]*)*")[\040\t]*(?:\([^\\\x80-\xff\n\015()]*(?:(?:\\[^\x80-\xff]|\([^\\\x80-\xff\n\015()]*(?:\\[^\x80-\xff][^\\\x80-\xff\n\015()]*)*\))[^\\\x80-\xff\n\015()]*)*\)[\040\t]*)*)*@[\040\t]*(?:\([^\\\x80-\xff\n\015()]*(?:(?:\\[^\x80-\xff]|\([^\\\x80-\xff\n\015()]*(?:\\[^\x80-\xff][^\\\x80-\xff\n\015()]*)*\))[^\\\x80-\xff\n\015()]*)*\)[\040\t]*)*(?:[^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff]+(?![^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff])|\[(?:[^\\\x80-\xff\n\015\[\]]|\\[^\x80-\xff])*\])[\040\t]*(?:\([^\\\x80-\xff\n\015()]*(?:(?:\\[^\x80-\xff]|\([^\\\x80-\xff\n\015()]*(?:\\[^\x80-\xff][^\\\x80-\xff\n\015()]*)*\))[^\\\x80-\xff\n\015()]*)*\)[\040\t]*)*(?:\.[\040\t]*(?:\([^\\\x80-\xff\n\015()]*(?:(?:\\[^\x80-\xff]|\([^\\\x80-\xff\n\015()]*(?:\\[^\x80-\xff][^\\\x80-\xff\n\015()]*)*\))[^\\\x80-\xff\n\015()]*)*\)[\040\t]*)*(?:[^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff]+(?![^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff])|\[(?:[^\\\x80-\xff\n\015\[\]]|\\[^\x80-\xff])*\])[\040\t]*(?:\([^\\\x80-\xff\n\015()]*(?:(?:\\[^\x80-\xff]|\([^\\\x80-\xff\n\015()]*(?:\\[^\x80-\xff][^\\\x80-\xff\n\015()]*)*\))[^\\\x80-\xff\n\015()]*)*\)[\040\t]*)*)*|(?:[^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff]+(?![^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff])|"[^\\\x80-\xff\n\015"]*(?:\\[^\x80-\xff][^\\\x80-\xff\n\015"]*)*")[^()<>@,;:".\\\[\]\x80-\xff\000-\010\012-\037]*(?:(?:\([^\\\x80-\xff\n\015()]*(?:(?:\\[^\x80-\xff]|\([^\\\x80-\xff\n\015()]*(?:\\[^\x80-\xff][^\\\x80-\xff\n\015()]*)*\))[^\\\x80-\xff\n\015()]*)*\)|"[^\\\x80-\xff\n\015"]*(?:\\[^\x80-\xff][^\\\x80-\xff\n\015"]*)*")[^()<>@,;:".\\\[\]\x80-\xff\000-\010\012-\037]*)*<[\040\t]*(?:\([^\\\x80-\xff\n\015()]*(?:(?:\\[^\x80-\xff]|\([^\\\x80-\xff\n\015()]*(?:\\[^\x80-\xff][^\\\x80-\xff\n\015()]*)*\))[^\\\x80-\xff\n\015()]*)*\)[\040\t]*)*(?:@[\040\t]*(?:\([^\\\x80-\xff\n\015()]*(?:(?:\\[^\x80-\xff]|\([^\\\x80-\xff\n\015()]*(?:\\[^\x80-\xff][^\\\x80-\xff\n\015()]*)*\))[^\\\x80-\xff\n\015()]*)*\)[\040\t]*)*(?:[^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff]+(?![^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff])|\[(?:[^\\\x80-\xff\n\015\[\]]|\\[^\x80-\xff])*\])[\040\t]*(?:\([^\\\x80-\xff\n\015()]*(?:(?:\\[^\x80-\xff]|\([^\\\x80-\xff\n\015()]*(?:\\[^\x80-\xff][^\\\x80-\xff\n\015()]*)*\))[^\\\x80-\xff\n\015()]*)*\)[\040\t]*)*(?:\.[\040\t]*(?:\([^\\\x80-\xff\n\015()]*(?:(?:\\[^\x80-\xff]|\([^\\\x80-\xff\n\015()]*(?:\\[^\x80-\xff][^\\\x80-\xff\n\015()]*)*\))[^\\\x80-\xff\n\015()]*)*\)[\040\t]*)*(?:[^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff]+(?![^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff])|\[(?:[^\\\x80-\xff\n\015\[\]]|\\[^\x80-\xff])*\])[\040\t]*(?:\([^\\\x80-\xff\n\015()]*(?:(?:\\[^\x80-\xff]|\([^\\\x80-\xff\n\015()]*(?:\\[^\x80-\xff][^\\\x80-\xff\n\015()]*)*\))[^\\\x80-\xff\n\015()]*)*\)[\040\t]*)*)*(?:,[\040\t]*(?:\([^\\\x80-\xff\n\015()]*(?:(?:\\[^\x80-\xff]|\([^\\\x80-\xff\n\015()]*(?:\\[^\x80-\xff][^\\\x80-\xff\n\015()]*)*\))[^\\\x80-\xff\n\015()]*)*\)[\040\t]*)*@[\040\t]*(?:\([^\\\x80-\xff\n\015()]*(?:(?:\\[^\x80-\xff]|\([^\\\x80-\xff\n\015()]*(?:\\[^\x80-\xff][^\\\x80-\xff\n\015()]*)*\))[^\\\x80-\xff\n\015()]*)*\)[\040\t]*)*(?:[^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff]+(?![^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff])|\[(?:[^\\\x80-\xff\n\015\[\]]|\\[^\x80-\xff])*\])[\040\t]*(?:\([^\\\x80-\xff\n\015()]*(?:(?:\\[^\x80-\xff]|\([^\\\x80-\xff\n\015()]*(?:\\[^\x80-\xff][^\\\x80-\xff\n\015()]*)*\))[^\\\x80-\xff\n\015()]*)*\)[\040\t]*)*(?:\.[\040\t]*(?:\([^\\\x80-\xff\n\015()]*(?:(?:\\[^\x80-\xff]|\([^\\\x80-\xff\n\015()]*(?:\\[^\x80-\xff][^\\\x80-\xff\n\015()]*)*\))[^\\\x80-\xff\n\015()]*)*\)[\040\t]*)*(?:[^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff]+(?![^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff])|\[(?:[^\\\x80-\xff\n\015\[\]]|\\[^\x80-\xff])*\])[\040\t]*(?:\([^\\\x80-\xff\n\015()]*(?:(?:\\[^\x80-\xff]|\([^\\\x80-\xff\n\015()]*(?:\\[^\x80-\xff][^\\\x80-\xff\n\015()]*)*\))[^\\\x80-\xff\n\015()]*)*\)[\040\t]*)*)*)*:[\040\t]*(?:\([^\\\x80-\xff\n\015()]*(?:(?:\\[^\x80-\xff]|\([^\\\x80-\xff\n\015()]*(?:\\[^\x80-\xff][^\\\x80-\xff\n\015()]*)*\))[^\\\x80-\xff\n\015()]*)*\)[\040\t]*)*)?(?:[^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff]+(?![^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff])|"[^\\\x80-\xff\n\015"]*(?:\\[^\x80-\xff][^\\\x80-\xff\n\015"]*)*")[\040\t]*(?:\([^\\\x80-\xff\n\015()]*(?:(?:\\[^\x80-\xff]|\([^\\\x80-\xff\n\015()]*(?:\\[^\x80-\xff][^\\\x80-\xff\n\015()]*)*\))[^\\\x80-\xff\n\015()]*)*\)[\040\t]*)*(?:\.[\040\t]*(?:\([^\\\x80-\xff\n\015()]*(?:(?:\\[^\x80-\xff]|\([^\\\x80-\xff\n\015()]*(?:\\[^\x80-\xff][^\\\x80-\xff\n\015()]*)*\))[^\\\x80-\xff\n\015()]*)*\)[\040\t]*)*(?:[^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff]+(?![^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff])|"[^\\\x80-\xff\n\015"]*(?:\\[^\x80-\xff][^\\\x80-\xff\n\015"]*)*")[\040\t]*(?:\([^\\\x80-\xff\n\015()]*(?:(?:\\[^\x80-\xff]|\([^\\\x80-\xff\n\015()]*(?:\\[^\x80-\xff][^\\\x80-\xff\n\015()]*)*\))[^\\\x80-\xff\n\015()]*)*\)[\040\t]*)*)*@[\040\t]*(?:\([^\\\x80-\xff\n\015()]*(?:(?:\\[^\x80-\xff]|\([^\\\x80-\xff\n\015()]*(?:\\[^\x80-\xff][^\\\x80-\xff\n\015()]*)*\))[^\\\x80-\xff\n\015()]*)*\)[\040\t]*)*(?:[^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff]+(?![^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff])|\[(?:[^\\\x80-\xff\n\015\[\]]|\\[^\x80-\xff])*\])[\040\t]*(?:\([^\\\x80-\xff\n\015()]*(?:(?:\\[^\x80-\xff]|\([^\\\x80-\xff\n\015()]*(?:\\[^\x80-\xff][^\\\x80-\xff\n\015()]*)*\))[^\\\x80-\xff\n\015()]*)*\)[\040\t]*)*(?:\.[\040\t]*(?:\([^\\\x80-\xff\n\015()]*(?:(?:\\[^\x80-\xff]|\([^\\\x80-\xff\n\015()]*(?:\\[^\x80-\xff][^\\\x80-\xff\n\015()]*)*\))[^\\\x80-\xff\n\015()]*)*\)[\040\t]*)*(?:[^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff]+(?![^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff])|\[(?:[^\\\x80-\xff\n\015\[\]]|\\[^\x80-\xff])*\])[\040\t]*(?:\([^\\\x80-\xff\n\015()]*(?:(?:\\[^\x80-\xff]|\([^\\\x80-\xff\n\015()]*(?:\\[^\x80-\xff][^\\\x80-\xff\n\015()]*)*\))[^\\\x80-\xff\n\015()]*)*\)[\040\t]*)*)*>)
    // Skipping Unicode-unfriendly [\040\t]*(?:\([^\\\x80-\xff\n\015()]*(?:(?:\\[^\x80-\xff]|\([^\\\x80-\xff\n\015()]*(?:\\[^\x80-\xff][^\\\x80-\xff\n\015()]*)*\))[^\\\x80-\xff\n\015()]*)*\)[\040\t]*)*(?:(?:[^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff]+(?![^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff])|"[^\\\x80-\xff\n\015"]*(?:\\[^\x80-\xff][^\\\x80-\xff\n\015"]*)*")[\040\t]*(?:\([^\\\x80-\xff\n\015()]*(?:(?:\\[^\x80-\xff]|\([^\\\x80-\xff\n\015()]*(?:\\[^\x80-\xff][^\\\x80-\xff\n\015()]*)*\))[^\\\x80-\xff\n\015()]*)*\)[\040\t]*)*(?:\.[\040\t]*(?:\([^\\\x80-\xff\n\015()]*(?:(?:\\[^\x80-\xff]|\([^\\\x80-\xff\n\015()]*(?:\\[^\x80-\xff][^\\\x80-\xff\n\015()]*)*\))[^\\\x80-\xff\n\015()]*)*\)[\040\t]*)*(?:[^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff]+(?![^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff])|"[^\\\x80-\xff\n\015"]*(?:\\[^\x80-\xff][^\\\x80-\xff\n\015"]*)*")[\040\t]*(?:\([^\\\x80-\xff\n\015()]*(?:(?:\\[^\x80-\xff]|\([^\\\x80-\xff\n\015()]*(?:\\[^\x80-\xff][^\\\x80-\xff\n\015()]*)*\))[^\\\x80-\xff\n\015()]*)*\)[\040\t]*)*)*@[\040\t]*(?:\([^\\\x80-\xff\n\015()]*(?:(?:\\[^\x80-\xff]|\([^\\\x80-\xff\n\015()]*(?:\\[^\x80-\xff][^\\\x80-\xff\n\015()]*)*\))[^\\\x80-\xff\n\015()]*)*\)[\040\t]*)*(?:[^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff]+(?![^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff])|\[(?:[^\\\x80-\xff\n\015\[\]]|\\[^\x80-\xff])*\])[\040\t]*(?:\([^\\\x80-\xff\n\015()]*(?:(?:\\[^\x80-\xff]|\([^\\\x80-\xff\n\015()]*(?:\\[^\x80-\xff][^\\\x80-\xff\n\015()]*)*\))[^\\\x80-\xff\n\015()]*)*\)[\040\t]*)*(?:\.[\040\t]*(?:\([^\\\x80-\xff\n\015()]*(?:(?:\\[^\x80-\xff]|\([^\\\x80-\xff\n\015()]*(?:\\[^\x80-\xff][^\\\x80-\xff\n\015()]*)*\))[^\\\x80-\xff\n\015()]*)*\)[\040\t]*)*(?:[^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff]+(?![^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff])|\[(?:[^\\\x80-\xff\n\015\[\]]|\\[^\x80-\xff])*\])[\040\t]*(?:\([^\\\x80-\xff\n\015()]*(?:(?:\\[^\x80-\xff]|\([^\\\x80-\xff\n\015()]*(?:\\[^\x80-\xff][^\\\x80-\xff\n\015()]*)*\))[^\\\x80-\xff\n\015()]*)*\)[\040\t]*)*)*|(?:[^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff]+(?![^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff])|"[^\\\x80-\xff\n\015"]*(?:\\[^\x80-\xff][^\\\x80-\xff\n\015"]*)*")[^()<>@,;:".\\\[\]\x80-\xff\000-\010\012-\037]*(?:(?:\([^\\\x80-\xff\n\015()]*(?:(?:\\[^\x80-\xff]|\([^\\\x80-\xff\n\015()]*(?:\\[^\x80-\xff][^\\\x80-\xff\n\015()]*)*\))[^\\\x80-\xff\n\015()]*)*\)|"[^\\\x80-\xff\n\015"]*(?:\\[^\x80-\xff][^\\\x80-\xff\n\015"]*)*")[^()<>@,;:".\\\[\]\x80-\xff\000-\010\012-\037]*)*<[\040\t]*(?:\([^\\\x80-\xff\n\015()]*(?:(?:\\[^\x80-\xff]|\([^\\\x80-\xff\n\015()]*(?:\\[^\x80-\xff][^\\\x80-\xff\n\015()]*)*\))[^\\\x80-\xff\n\015()]*)*\)[\040\t]*)*(?:@[\040\t]*(?:\([^\\\x80-\xff\n\015()]*(?:(?:\\[^\x80-\xff]|\([^\\\x80-\xff\n\015()]*(?:\\[^\x80-\xff][^\\\x80-\xff\n\015()]*)*\))[^\\\x80-\xff\n\015()]*)*\)[\040\t]*)*(?:[^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff]+(?![^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff])|\[(?:[^\\\x80-\xff\n\015\[\]]|\\[^\x80-\xff])*\])[\040\t]*(?:\([^\\\x80-\xff\n\015()]*(?:(?:\\[^\x80-\xff]|\([^\\\x80-\xff\n\015()]*(?:\\[^\x80-\xff][^\\\x80-\xff\n\015()]*)*\))[^\\\x80-\xff\n\015()]*)*\)[\040\t]*)*(?:\.[\040\t]*(?:\([^\\\x80-\xff\n\015()]*(?:(?:\\[^\x80-\xff]|\([^\\\x80-\xff\n\015()]*(?:\\[^\x80-\xff][^\\\x80-\xff\n\015()]*)*\))[^\\\x80-\xff\n\015()]*)*\)[\040\t]*)*(?:[^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff]+(?![^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff])|\[(?:[^\\\x80-\xff\n\015\[\]]|\\[^\x80-\xff])*\])[\040\t]*(?:\([^\\\x80-\xff\n\015()]*(?:(?:\\[^\x80-\xff]|\([^\\\x80-\xff\n\015()]*(?:\\[^\x80-\xff][^\\\x80-\xff\n\015()]*)*\))[^\\\x80-\xff\n\015()]*)*\)[\040\t]*)*)*(?:,[\040\t]*(?:\([^\\\x80-\xff\n\015()]*(?:(?:\\[^\x80-\xff]|\([^\\\x80-\xff\n\015()]*(?:\\[^\x80-\xff][^\\\x80-\xff\n\015()]*)*\))[^\\\x80-\xff\n\015()]*)*\)[\040\t]*)*@[\040\t]*(?:\([^\\\x80-\xff\n\015()]*(?:(?:\\[^\x80-\xff]|\([^\\\x80-\xff\n\015()]*(?:\\[^\x80-\xff][^\\\x80-\xff\n\015()]*)*\))[^\\\x80-\xff\n\015()]*)*\)[\040\t]*)*(?:[^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff]+(?![^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff])|\[(?:[^\\\x80-\xff\n\015\[\]]|\\[^\x80-\xff])*\])[\040\t]*(?:\([^\\\x80-\xff\n\015()]*(?:(?:\\[^\x80-\xff]|\([^\\\x80-\xff\n\015()]*(?:\\[^\x80-\xff][^\\\x80-\xff\n\015()]*)*\))[^\\\x80-\xff\n\015()]*)*\)[\040\t]*)*(?:\.[\040\t]*(?:\([^\\\x80-\xff\n\015()]*(?:(?:\\[^\x80-\xff]|\([^\\\x80-\xff\n\015()]*(?:\\[^\x80-\xff][^\\\x80-\xff\n\015()]*)*\))[^\\\x80-\xff\n\015()]*)*\)[\040\t]*)*(?:[^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff]+(?![^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff])|\[(?:[^\\\x80-\xff\n\015\[\]]|\\[^\x80-\xff])*\])[\040\t]*(?:\([^\\\x80-\xff\n\015()]*(?:(?:\\[^\x80-\xff]|\([^\\\x80-\xff\n\015()]*(?:\\[^\x80-\xff][^\\\x80-\xff\n\015()]*)*\))[^\\\x80-\xff\n\015()]*)*\)[\040\t]*)*)*)*:[\040\t]*(?:\([^\\\x80-\xff\n\015()]*(?:(?:\\[^\x80-\xff]|\([^\\\x80-\xff\n\015()]*(?:\\[^\x80-\xff][^\\\x80-\xff\n\015()]*)*\))[^\\\x80-\xff\n\015()]*)*\)[\040\t]*)*)?(?:[^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff]+(?![^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff])|"[^\\\x80-\xff\n\015"]*(?:\\[^\x80-\xff][^\\\x80-\xff\n\015"]*)*")[\040\t]*(?:\([^\\\x80-\xff\n\015()]*(?:(?:\\[^\x80-\xff]|\([^\\\x80-\xff\n\015()]*(?:\\[^\x80-\xff][^\\\x80-\xff\n\015()]*)*\))[^\\\x80-\xff\n\015()]*)*\)[\040\t]*)*(?:\.[\040\t]*(?:\([^\\\x80-\xff\n\015()]*(?:(?:\\[^\x80-\xff]|\([^\\\x80-\xff\n\015()]*(?:\\[^\x80-\xff][^\\\x80-\xff\n\015()]*)*\))[^\\\x80-\xff\n\015()]*)*\)[\040\t]*)*(?:[^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff]+(?![^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff])|"[^\\\x80-\xff\n\015"]*(?:\\[^\x80-\xff][^\\\x80-\xff\n\015"]*)*")[\040\t]*(?:\([^\\\x80-\xff\n\015()]*(?:(?:\\[^\x80-\xff]|\([^\\\x80-\xff\n\015()]*(?:\\[^\x80-\xff][^\\\x80-\xff\n\015()]*)*\))[^\\\x80-\xff\n\015()]*)*\)[\040\t]*)*)*@[\040\t]*(?:\([^\\\x80-\xff\n\015()]*(?:(?:\\[^\x80-\xff]|\([^\\\x80-\xff\n\015()]*(?:\\[^\x80-\xff][^\\\x80-\xff\n\015()]*)*\))[^\\\x80-\xff\n\015()]*)*\)[\040\t]*)*(?:[^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff]+(?![^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff])|\[(?:[^\\\x80-\xff\n\015\[\]]|\\[^\x80-\xff])*\])[\040\t]*(?:\([^\\\x80-\xff\n\015()]*(?:(?:\\[^\x80-\xff]|\([^\\\x80-\xff\n\015()]*(?:\\[^\x80-\xff][^\\\x80-\xff\n\015()]*)*\))[^\\\x80-\xff\n\015()]*)*\)[\040\t]*)*(?:\.[\040\t]*(?:\([^\\\x80-\xff\n\015()]*(?:(?:\\[^\x80-\xff]|\([^\\\x80-\xff\n\015()]*(?:\\[^\x80-\xff][^\\\x80-\xff\n\015()]*)*\))[^\\\x80-\xff\n\015()]*)*\)[\040\t]*)*(?:[^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff]+(?![^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff])|\[(?:[^\\\x80-\xff\n\015\[\]]|\\[^\x80-\xff])*\])[\040\t]*(?:\([^\\\x80-\xff\n\015()]*(?:(?:\\[^\x80-\xff]|\([^\\\x80-\xff\n\015()]*(?:\\[^\x80-\xff][^\\\x80-\xff\n\015()]*)*\))[^\\\x80-\xff\n\015()]*)*\)[\040\t]*)*)*>)
    // Skipping Unicode-unfriendly [\040\t]*(?:\([^\\\x80-\xff\n\015()]*(?:(?:\\[^\x80-\xff]|\([^\\\x80-\xff\n\015()]*(?:\\[^\x80-\xff][^\\\x80-\xff\n\015()]*)*\))[^\\\x80-\xff\n\015()]*)*\)[\040\t]*)*(?:(?:[^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff]+(?![^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff])|"[^\\\x80-\xff\n\015"]*(?:\\[^\x80-\xff][^\\\x80-\xff\n\015"]*)*")[\040\t]*(?:\([^\\\x80-\xff\n\015()]*(?:(?:\\[^\x80-\xff]|\([^\\\x80-\xff\n\015()]*(?:\\[^\x80-\xff][^\\\x80-\xff\n\015()]*)*\))[^\\\x80-\xff\n\015()]*)*\)[\040\t]*)*(?:\.[\040\t]*(?:\([^\\\x80-\xff\n\015()]*(?:(?:\\[^\x80-\xff]|\([^\\\x80-\xff\n\015()]*(?:\\[^\x80-\xff][^\\\x80-\xff\n\015()]*)*\))[^\\\x80-\xff\n\015()]*)*\)[\040\t]*)*(?:[^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff]+(?![^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff])|"[^\\\x80-\xff\n\015"]*(?:\\[^\x80-\xff][^\\\x80-\xff\n\015"]*)*")[\040\t]*(?:\([^\\\x80-\xff\n\015()]*(?:(?:\\[^\x80-\xff]|\([^\\\x80-\xff\n\015()]*(?:\\[^\x80-\xff][^\\\x80-\xff\n\015()]*)*\))[^\\\x80-\xff\n\015()]*)*\)[\040\t]*)*)*@[\040\t]*(?:\([^\\\x80-\xff\n\015()]*(?:(?:\\[^\x80-\xff]|\([^\\\x80-\xff\n\015()]*(?:\\[^\x80-\xff][^\\\x80-\xff\n\015()]*)*\))[^\\\x80-\xff\n\015()]*)*\)[\040\t]*)*(?:[^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff]+(?![^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff])|\[(?:[^\\\x80-\xff\n\015\[\]]|\\[^\x80-\xff])*\])[\040\t]*(?:\([^\\\x80-\xff\n\015()]*(?:(?:\\[^\x80-\xff]|\([^\\\x80-\xff\n\015()]*(?:\\[^\x80-\xff][^\\\x80-\xff\n\015()]*)*\))[^\\\x80-\xff\n\015()]*)*\)[\040\t]*)*(?:\.[\040\t]*(?:\([^\\\x80-\xff\n\015()]*(?:(?:\\[^\x80-\xff]|\([^\\\x80-\xff\n\015()]*(?:\\[^\x80-\xff][^\\\x80-\xff\n\015()]*)*\))[^\\\x80-\xff\n\015()]*)*\)[\040\t]*)*(?:[^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff]+(?![^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff])|\[(?:[^\\\x80-\xff\n\015\[\]]|\\[^\x80-\xff])*\])[\040\t]*(?:\([^\\\x80-\xff\n\015()]*(?:(?:\\[^\x80-\xff]|\([^\\\x80-\xff\n\015()]*(?:\\[^\x80-\xff][^\\\x80-\xff\n\015()]*)*\))[^\\\x80-\xff\n\015()]*)*\)[\040\t]*)*)*|(?:[^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff]+(?![^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff])|"[^\\\x80-\xff\n\015"]*(?:\\[^\x80-\xff][^\\\x80-\xff\n\015"]*)*")[^()<>@,;:".\\\[\]\x80-\xff\000-\010\012-\037]*(?:(?:\([^\\\x80-\xff\n\015()]*(?:(?:\\[^\x80-\xff]|\([^\\\x80-\xff\n\015()]*(?:\\[^\x80-\xff][^\\\x80-\xff\n\015()]*)*\))[^\\\x80-\xff\n\015()]*)*\)|"[^\\\x80-\xff\n\015"]*(?:\\[^\x80-\xff][^\\\x80-\xff\n\015"]*)*")[^()<>@,;:".\\\[\]\x80-\xff\000-\010\012-\037]*)*<[\040\t]*(?:\([^\\\x80-\xff\n\015()]*(?:(?:\\[^\x80-\xff]|\([^\\\x80-\xff\n\015()]*(?:\\[^\x80-\xff][^\\\x80-\xff\n\015()]*)*\))[^\\\x80-\xff\n\015()]*)*\)[\040\t]*)*(?:@[\040\t]*(?:\([^\\\x80-\xff\n\015()]*(?:(?:\\[^\x80-\xff]|\([^\\\x80-\xff\n\015()]*(?:\\[^\x80-\xff][^\\\x80-\xff\n\015()]*)*\))[^\\\x80-\xff\n\015()]*)*\)[\040\t]*)*(?:[^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff]+(?![^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff])|\[(?:[^\\\x80-\xff\n\015\[\]]|\\[^\x80-\xff])*\])[\040\t]*(?:\([^\\\x80-\xff\n\015()]*(?:(?:\\[^\x80-\xff]|\([^\\\x80-\xff\n\015()]*(?:\\[^\x80-\xff][^\\\x80-\xff\n\015()]*)*\))[^\\\x80-\xff\n\015()]*)*\)[\040\t]*)*(?:\.[\040\t]*(?:\([^\\\x80-\xff\n\015()]*(?:(?:\\[^\x80-\xff]|\([^\\\x80-\xff\n\015()]*(?:\\[^\x80-\xff][^\\\x80-\xff\n\015()]*)*\))[^\\\x80-\xff\n\015()]*)*\)[\040\t]*)*(?:[^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff]+(?![^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff])|\[(?:[^\\\x80-\xff\n\015\[\]]|\\[^\x80-\xff])*\])[\040\t]*(?:\([^\\\x80-\xff\n\015()]*(?:(?:\\[^\x80-\xff]|\([^\\\x80-\xff\n\015()]*(?:\\[^\x80-\xff][^\\\x80-\xff\n\015()]*)*\))[^\\\x80-\xff\n\015()]*)*\)[\040\t]*)*)*(?:,[\040\t]*(?:\([^\\\x80-\xff\n\015()]*(?:(?:\\[^\x80-\xff]|\([^\\\x80-\xff\n\015()]*(?:\\[^\x80-\xff][^\\\x80-\xff\n\015()]*)*\))[^\\\x80-\xff\n\015()]*)*\)[\040\t]*)*@[\040\t]*(?:\([^\\\x80-\xff\n\015()]*(?:(?:\\[^\x80-\xff]|\([^\\\x80-\xff\n\015()]*(?:\\[^\x80-\xff][^\\\x80-\xff\n\015()]*)*\))[^\\\x80-\xff\n\015()]*)*\)[\040\t]*)*(?:[^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff]+(?![^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff])|\[(?:[^\\\x80-\xff\n\015\[\]]|\\[^\x80-\xff])*\])[\040\t]*(?:\([^\\\x80-\xff\n\015()]*(?:(?:\\[^\x80-\xff]|\([^\\\x80-\xff\n\015()]*(?:\\[^\x80-\xff][^\\\x80-\xff\n\015()]*)*\))[^\\\x80-\xff\n\015()]*)*\)[\040\t]*)*(?:\.[\040\t]*(?:\([^\\\x80-\xff\n\015()]*(?:(?:\\[^\x80-\xff]|\([^\\\x80-\xff\n\015()]*(?:\\[^\x80-\xff][^\\\x80-\xff\n\015()]*)*\))[^\\\x80-\xff\n\015()]*)*\)[\040\t]*)*(?:[^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff]+(?![^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff])|\[(?:[^\\\x80-\xff\n\015\[\]]|\\[^\x80-\xff])*\])[\040\t]*(?:\([^\\\x80-\xff\n\015()]*(?:(?:\\[^\x80-\xff]|\([^\\\x80-\xff\n\015()]*(?:\\[^\x80-\xff][^\\\x80-\xff\n\015()]*)*\))[^\\\x80-\xff\n\015()]*)*\)[\040\t]*)*)*)*:[\040\t]*(?:\([^\\\x80-\xff\n\015()]*(?:(?:\\[^\x80-\xff]|\([^\\\x80-\xff\n\015()]*(?:\\[^\x80-\xff][^\\\x80-\xff\n\015()]*)*\))[^\\\x80-\xff\n\015()]*)*\)[\040\t]*)*)?(?:[^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff]+(?![^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff])|"[^\\\x80-\xff\n\015"]*(?:\\[^\x80-\xff][^\\\x80-\xff\n\015"]*)*")[\040\t]*(?:\([^\\\x80-\xff\n\015()]*(?:(?:\\[^\x80-\xff]|\([^\\\x80-\xff\n\015()]*(?:\\[^\x80-\xff][^\\\x80-\xff\n\015()]*)*\))[^\\\x80-\xff\n\015()]*)*\)[\040\t]*)*(?:\.[\040\t]*(?:\([^\\\x80-\xff\n\015()]*(?:(?:\\[^\x80-\xff]|\([^\\\x80-\xff\n\015()]*(?:\\[^\x80-\xff][^\\\x80-\xff\n\015()]*)*\))[^\\\x80-\xff\n\015()]*)*\)[\040\t]*)*(?:[^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff]+(?![^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff])|"[^\\\x80-\xff\n\015"]*(?:\\[^\x80-\xff][^\\\x80-\xff\n\015"]*)*")[\040\t]*(?:\([^\\\x80-\xff\n\015()]*(?:(?:\\[^\x80-\xff]|\([^\\\x80-\xff\n\015()]*(?:\\[^\x80-\xff][^\\\x80-\xff\n\015()]*)*\))[^\\\x80-\xff\n\015()]*)*\)[\040\t]*)*)*@[\040\t]*(?:\([^\\\x80-\xff\n\015()]*(?:(?:\\[^\x80-\xff]|\([^\\\x80-\xff\n\015()]*(?:\\[^\x80-\xff][^\\\x80-\xff\n\015()]*)*\))[^\\\x80-\xff\n\015()]*)*\)[\040\t]*)*(?:[^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff]+(?![^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff])|\[(?:[^\\\x80-\xff\n\015\[\]]|\\[^\x80-\xff])*\])[\040\t]*(?:\([^\\\x80-\xff\n\015()]*(?:(?:\\[^\x80-\xff]|\([^\\\x80-\xff\n\015()]*(?:\\[^\x80-\xff][^\\\x80-\xff\n\015()]*)*\))[^\\\x80-\xff\n\015()]*)*\)[\040\t]*)*(?:\.[\040\t]*(?:\([^\\\x80-\xff\n\015()]*(?:(?:\\[^\x80-\xff]|\([^\\\x80-\xff\n\015()]*(?:\\[^\x80-\xff][^\\\x80-\xff\n\015()]*)*\))[^\\\x80-\xff\n\015()]*)*\)[\040\t]*)*(?:[^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff]+(?![^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff])|\[(?:[^\\\x80-\xff\n\015\[\]]|\\[^\x80-\xff])*\])[\040\t]*(?:\([^\\\x80-\xff\n\015()]*(?:(?:\\[^\x80-\xff]|\([^\\\x80-\xff\n\015()]*(?:\\[^\x80-\xff][^\\\x80-\xff\n\015()]*)*\))[^\\\x80-\xff\n\015()]*)*\)[\040\t]*)*)*>)
    // Skipping Unicode-unfriendly [\040\t]*(?:\([^\\\x80-\xff\n\015()]*(?:(?:\\[^\x80-\xff]|\([^\\\x80-\xff\n\015()]*(?:\\[^\x80-\xff][^\\\x80-\xff\n\015()]*)*\))[^\\\x80-\xff\n\015()]*)*\)[\040\t]*)*(?:(?:[^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff]+(?![^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff])|"[^\\\x80-\xff\n\015"]*(?:\\[^\x80-\xff][^\\\x80-\xff\n\015"]*)*")[\040\t]*(?:\([^\\\x80-\xff\n\015()]*(?:(?:\\[^\x80-\xff]|\([^\\\x80-\xff\n\015()]*(?:\\[^\x80-\xff][^\\\x80-\xff\n\015()]*)*\))[^\\\x80-\xff\n\015()]*)*\)[\040\t]*)*(?:\.[\040\t]*(?:\([^\\\x80-\xff\n\015()]*(?:(?:\\[^\x80-\xff]|\([^\\\x80-\xff\n\015()]*(?:\\[^\x80-\xff][^\\\x80-\xff\n\015()]*)*\))[^\\\x80-\xff\n\015()]*)*\)[\040\t]*)*(?:[^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff]+(?![^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff])|"[^\\\x80-\xff\n\015"]*(?:\\[^\x80-\xff][^\\\x80-\xff\n\015"]*)*")[\040\t]*(?:\([^\\\x80-\xff\n\015()]*(?:(?:\\[^\x80-\xff]|\([^\\\x80-\xff\n\015()]*(?:\\[^\x80-\xff][^\\\x80-\xff\n\015()]*)*\))[^\\\x80-\xff\n\015()]*)*\)[\040\t]*)*)*@[\040\t]*(?:\([^\\\x80-\xff\n\015()]*(?:(?:\\[^\x80-\xff]|\([^\\\x80-\xff\n\015()]*(?:\\[^\x80-\xff][^\\\x80-\xff\n\015()]*)*\))[^\\\x80-\xff\n\015()]*)*\)[\040\t]*)*(?:[^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff]+(?![^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff])|\[(?:[^\\\x80-\xff\n\015\[\]]|\\[^\x80-\xff])*\])[\040\t]*(?:\([^\\\x80-\xff\n\015()]*(?:(?:\\[^\x80-\xff]|\([^\\\x80-\xff\n\015()]*(?:\\[^\x80-\xff][^\\\x80-\xff\n\015()]*)*\))[^\\\x80-\xff\n\015()]*)*\)[\040\t]*)*(?:\.[\040\t]*(?:\([^\\\x80-\xff\n\015()]*(?:(?:\\[^\x80-\xff]|\([^\\\x80-\xff\n\015()]*(?:\\[^\x80-\xff][^\\\x80-\xff\n\015()]*)*\))[^\\\x80-\xff\n\015()]*)*\)[\040\t]*)*(?:[^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff]+(?![^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff])|\[(?:[^\\\x80-\xff\n\015\[\]]|\\[^\x80-\xff])*\])[\040\t]*(?:\([^\\\x80-\xff\n\015()]*(?:(?:\\[^\x80-\xff]|\([^\\\x80-\xff\n\015()]*(?:\\[^\x80-\xff][^\\\x80-\xff\n\015()]*)*\))[^\\\x80-\xff\n\015()]*)*\)[\040\t]*)*)*|(?:[^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff]+(?![^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff])|"[^\\\x80-\xff\n\015"]*(?:\\[^\x80-\xff][^\\\x80-\xff\n\015"]*)*")[^()<>@,;:".\\\[\]\x80-\xff\000-\010\012-\037]*(?:(?:\([^\\\x80-\xff\n\015()]*(?:(?:\\[^\x80-\xff]|\([^\\\x80-\xff\n\015()]*(?:\\[^\x80-\xff][^\\\x80-\xff\n\015()]*)*\))[^\\\x80-\xff\n\015()]*)*\)|"[^\\\x80-\xff\n\015"]*(?:\\[^\x80-\xff][^\\\x80-\xff\n\015"]*)*")[^()<>@,;:".\\\[\]\x80-\xff\000-\010\012-\037]*)*<[\040\t]*(?:\([^\\\x80-\xff\n\015()]*(?:(?:\\[^\x80-\xff]|\([^\\\x80-\xff\n\015()]*(?:\\[^\x80-\xff][^\\\x80-\xff\n\015()]*)*\))[^\\\x80-\xff\n\015()]*)*\)[\040\t]*)*(?:@[\040\t]*(?:\([^\\\x80-\xff\n\015()]*(?:(?:\\[^\x80-\xff]|\([^\\\x80-\xff\n\015()]*(?:\\[^\x80-\xff][^\\\x80-\xff\n\015()]*)*\))[^\\\x80-\xff\n\015()]*)*\)[\040\t]*)*(?:[^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff]+(?![^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff])|\[(?:[^\\\x80-\xff\n\015\[\]]|\\[^\x80-\xff])*\])[\040\t]*(?:\([^\\\x80-\xff\n\015()]*(?:(?:\\[^\x80-\xff]|\([^\\\x80-\xff\n\015()]*(?:\\[^\x80-\xff][^\\\x80-\xff\n\015()]*)*\))[^\\\x80-\xff\n\015()]*)*\)[\040\t]*)*(?:\.[\040\t]*(?:\([^\\\x80-\xff\n\015()]*(?:(?:\\[^\x80-\xff]|\([^\\\x80-\xff\n\015()]*(?:\\[^\x80-\xff][^\\\x80-\xff\n\015()]*)*\))[^\\\x80-\xff\n\015()]*)*\)[\040\t]*)*(?:[^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff]+(?![^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff])|\[(?:[^\\\x80-\xff\n\015\[\]]|\\[^\x80-\xff])*\])[\040\t]*(?:\([^\\\x80-\xff\n\015()]*(?:(?:\\[^\x80-\xff]|\([^\\\x80-\xff\n\015()]*(?:\\[^\x80-\xff][^\\\x80-\xff\n\015()]*)*\))[^\\\x80-\xff\n\015()]*)*\)[\040\t]*)*)*(?:,[\040\t]*(?:\([^\\\x80-\xff\n\015()]*(?:(?:\\[^\x80-\xff]|\([^\\\x80-\xff\n\015()]*(?:\\[^\x80-\xff][^\\\x80-\xff\n\015()]*)*\))[^\\\x80-\xff\n\015()]*)*\)[\040\t]*)*@[\040\t]*(?:\([^\\\x80-\xff\n\015()]*(?:(?:\\[^\x80-\xff]|\([^\\\x80-\xff\n\015()]*(?:\\[^\x80-\xff][^\\\x80-\xff\n\015()]*)*\))[^\\\x80-\xff\n\015()]*)*\)[\040\t]*)*(?:[^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff]+(?![^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff])|\[(?:[^\\\x80-\xff\n\015\[\]]|\\[^\x80-\xff])*\])[\040\t]*(?:\([^\\\x80-\xff\n\015()]*(?:(?:\\[^\x80-\xff]|\([^\\\x80-\xff\n\015()]*(?:\\[^\x80-\xff][^\\\x80-\xff\n\015()]*)*\))[^\\\x80-\xff\n\015()]*)*\)[\040\t]*)*(?:\.[\040\t]*(?:\([^\\\x80-\xff\n\015()]*(?:(?:\\[^\x80-\xff]|\([^\\\x80-\xff\n\015()]*(?:\\[^\x80-\xff][^\\\x80-\xff\n\015()]*)*\))[^\\\x80-\xff\n\015()]*)*\)[\040\t]*)*(?:[^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff]+(?![^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff])|\[(?:[^\\\x80-\xff\n\015\[\]]|\\[^\x80-\xff])*\])[\040\t]*(?:\([^\\\x80-\xff\n\015()]*(?:(?:\\[^\x80-\xff]|\([^\\\x80-\xff\n\015()]*(?:\\[^\x80-\xff][^\\\x80-\xff\n\015()]*)*\))[^\\\x80-\xff\n\015()]*)*\)[\040\t]*)*)*)*:[\040\t]*(?:\([^\\\x80-\xff\n\015()]*(?:(?:\\[^\x80-\xff]|\([^\\\x80-\xff\n\015()]*(?:\\[^\x80-\xff][^\\\x80-\xff\n\015()]*)*\))[^\\\x80-\xff\n\015()]*)*\)[\040\t]*)*)?(?:[^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff]+(?![^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff])|"[^\\\x80-\xff\n\015"]*(?:\\[^\x80-\xff][^\\\x80-\xff\n\015"]*)*")[\040\t]*(?:\([^\\\x80-\xff\n\015()]*(?:(?:\\[^\x80-\xff]|\([^\\\x80-\xff\n\015()]*(?:\\[^\x80-\xff][^\\\x80-\xff\n\015()]*)*\))[^\\\x80-\xff\n\015()]*)*\)[\040\t]*)*(?:\.[\040\t]*(?:\([^\\\x80-\xff\n\015()]*(?:(?:\\[^\x80-\xff]|\([^\\\x80-\xff\n\015()]*(?:\\[^\x80-\xff][^\\\x80-\xff\n\015()]*)*\))[^\\\x80-\xff\n\015()]*)*\)[\040\t]*)*(?:[^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff]+(?![^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff])|"[^\\\x80-\xff\n\015"]*(?:\\[^\x80-\xff][^\\\x80-\xff\n\015"]*)*")[\040\t]*(?:\([^\\\x80-\xff\n\015()]*(?:(?:\\[^\x80-\xff]|\([^\\\x80-\xff\n\015()]*(?:\\[^\x80-\xff][^\\\x80-\xff\n\015()]*)*\))[^\\\x80-\xff\n\015()]*)*\)[\040\t]*)*)*@[\040\t]*(?:\([^\\\x80-\xff\n\015()]*(?:(?:\\[^\x80-\xff]|\([^\\\x80-\xff\n\015()]*(?:\\[^\x80-\xff][^\\\x80-\xff\n\015()]*)*\))[^\\\x80-\xff\n\015()]*)*\)[\040\t]*)*(?:[^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff]+(?![^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff])|\[(?:[^\\\x80-\xff\n\015\[\]]|\\[^\x80-\xff])*\])[\040\t]*(?:\([^\\\x80-\xff\n\015()]*(?:(?:\\[^\x80-\xff]|\([^\\\x80-\xff\n\015()]*(?:\\[^\x80-\xff][^\\\x80-\xff\n\015()]*)*\))[^\\\x80-\xff\n\015()]*)*\)[\040\t]*)*(?:\.[\040\t]*(?:\([^\\\x80-\xff\n\015()]*(?:(?:\\[^\x80-\xff]|\([^\\\x80-\xff\n\015()]*(?:\\[^\x80-\xff][^\\\x80-\xff\n\015()]*)*\))[^\\\x80-\xff\n\015()]*)*\)[\040\t]*)*(?:[^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff]+(?![^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff])|\[(?:[^\\\x80-\xff\n\015\[\]]|\\[^\x80-\xff])*\])[\040\t]*(?:\([^\\\x80-\xff\n\015()]*(?:(?:\\[^\x80-\xff]|\([^\\\x80-\xff\n\015()]*(?:\\[^\x80-\xff][^\\\x80-\xff\n\015()]*)*\))[^\\\x80-\xff\n\015()]*)*\)[\040\t]*)*)*>)
    // Skipping Unicode-unfriendly [\040\t]*(?:\([^\\\x80-\xff\n\015()]*(?:(?:\\[^\x80-\xff]|\([^\\\x80-\xff\n\015()]*(?:\\[^\x80-\xff][^\\\x80-\xff\n\015()]*)*\))[^\\\x80-\xff\n\015()]*)*\)[\040\t]*)*(?:(?:[^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff]+(?![^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff])|"[^\\\x80-\xff\n\015"]*(?:\\[^\x80-\xff][^\\\x80-\xff\n\015"]*)*")[\040\t]*(?:\([^\\\x80-\xff\n\015()]*(?:(?:\\[^\x80-\xff]|\([^\\\x80-\xff\n\015()]*(?:\\[^\x80-\xff][^\\\x80-\xff\n\015()]*)*\))[^\\\x80-\xff\n\015()]*)*\)[\040\t]*)*(?:\.[\040\t]*(?:\([^\\\x80-\xff\n\015()]*(?:(?:\\[^\x80-\xff]|\([^\\\x80-\xff\n\015()]*(?:\\[^\x80-\xff][^\\\x80-\xff\n\015()]*)*\))[^\\\x80-\xff\n\015()]*)*\)[\040\t]*)*(?:[^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff]+(?![^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff])|"[^\\\x80-\xff\n\015"]*(?:\\[^\x80-\xff][^\\\x80-\xff\n\015"]*)*")[\040\t]*(?:\([^\\\x80-\xff\n\015()]*(?:(?:\\[^\x80-\xff]|\([^\\\x80-\xff\n\015()]*(?:\\[^\x80-\xff][^\\\x80-\xff\n\015()]*)*\))[^\\\x80-\xff\n\015()]*)*\)[\040\t]*)*)*@[\040\t]*(?:\([^\\\x80-\xff\n\015()]*(?:(?:\\[^\x80-\xff]|\([^\\\x80-\xff\n\015()]*(?:\\[^\x80-\xff][^\\\x80-\xff\n\015()]*)*\))[^\\\x80-\xff\n\015()]*)*\)[\040\t]*)*(?:[^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff]+(?![^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff])|\[(?:[^\\\x80-\xff\n\015\[\]]|\\[^\x80-\xff])*\])[\040\t]*(?:\([^\\\x80-\xff\n\015()]*(?:(?:\\[^\x80-\xff]|\([^\\\x80-\xff\n\015()]*(?:\\[^\x80-\xff][^\\\x80-\xff\n\015()]*)*\))[^\\\x80-\xff\n\015()]*)*\)[\040\t]*)*(?:\.[\040\t]*(?:\([^\\\x80-\xff\n\015()]*(?:(?:\\[^\x80-\xff]|\([^\\\x80-\xff\n\015()]*(?:\\[^\x80-\xff][^\\\x80-\xff\n\015()]*)*\))[^\\\x80-\xff\n\015()]*)*\)[\040\t]*)*(?:[^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff]+(?![^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff])|\[(?:[^\\\x80-\xff\n\015\[\]]|\\[^\x80-\xff])*\])[\040\t]*(?:\([^\\\x80-\xff\n\015()]*(?:(?:\\[^\x80-\xff]|\([^\\\x80-\xff\n\015()]*(?:\\[^\x80-\xff][^\\\x80-\xff\n\015()]*)*\))[^\\\x80-\xff\n\015()]*)*\)[\040\t]*)*)*|(?:[^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff]+(?![^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff])|"[^\\\x80-\xff\n\015"]*(?:\\[^\x80-\xff][^\\\x80-\xff\n\015"]*)*")[^()<>@,;:".\\\[\]\x80-\xff\000-\010\012-\037]*(?:(?:\([^\\\x80-\xff\n\015()]*(?:(?:\\[^\x80-\xff]|\([^\\\x80-\xff\n\015()]*(?:\\[^\x80-\xff][^\\\x80-\xff\n\015()]*)*\))[^\\\x80-\xff\n\015()]*)*\)|"[^\\\x80-\xff\n\015"]*(?:\\[^\x80-\xff][^\\\x80-\xff\n\015"]*)*")[^()<>@,;:".\\\[\]\x80-\xff\000-\010\012-\037]*)*<[\040\t]*(?:\([^\\\x80-\xff\n\015()]*(?:(?:\\[^\x80-\xff]|\([^\\\x80-\xff\n\015()]*(?:\\[^\x80-\xff][^\\\x80-\xff\n\015()]*)*\))[^\\\x80-\xff\n\015()]*)*\)[\040\t]*)*(?:@[\040\t]*(?:\([^\\\x80-\xff\n\015()]*(?:(?:\\[^\x80-\xff]|\([^\\\x80-\xff\n\015()]*(?:\\[^\x80-\xff][^\\\x80-\xff\n\015()]*)*\))[^\\\x80-\xff\n\015()]*)*\)[\040\t]*)*(?:[^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff]+(?![^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff])|\[(?:[^\\\x80-\xff\n\015\[\]]|\\[^\x80-\xff])*\])[\040\t]*(?:\([^\\\x80-\xff\n\015()]*(?:(?:\\[^\x80-\xff]|\([^\\\x80-\xff\n\015()]*(?:\\[^\x80-\xff][^\\\x80-\xff\n\015()]*)*\))[^\\\x80-\xff\n\015()]*)*\)[\040\t]*)*(?:\.[\040\t]*(?:\([^\\\x80-\xff\n\015()]*(?:(?:\\[^\x80-\xff]|\([^\\\x80-\xff\n\015()]*(?:\\[^\x80-\xff][^\\\x80-\xff\n\015()]*)*\))[^\\\x80-\xff\n\015()]*)*\)[\040\t]*)*(?:[^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff]+(?![^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff])|\[(?:[^\\\x80-\xff\n\015\[\]]|\\[^\x80-\xff])*\])[\040\t]*(?:\([^\\\x80-\xff\n\015()]*(?:(?:\\[^\x80-\xff]|\([^\\\x80-\xff\n\015()]*(?:\\[^\x80-\xff][^\\\x80-\xff\n\015()]*)*\))[^\\\x80-\xff\n\015()]*)*\)[\040\t]*)*)*(?:,[\040\t]*(?:\([^\\\x80-\xff\n\015()]*(?:(?:\\[^\x80-\xff]|\([^\\\x80-\xff\n\015()]*(?:\\[^\x80-\xff][^\\\x80-\xff\n\015()]*)*\))[^\\\x80-\xff\n\015()]*)*\)[\040\t]*)*@[\040\t]*(?:\([^\\\x80-\xff\n\015()]*(?:(?:\\[^\x80-\xff]|\([^\\\x80-\xff\n\015()]*(?:\\[^\x80-\xff][^\\\x80-\xff\n\015()]*)*\))[^\\\x80-\xff\n\015()]*)*\)[\040\t]*)*(?:[^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff]+(?![^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff])|\[(?:[^\\\x80-\xff\n\015\[\]]|\\[^\x80-\xff])*\])[\040\t]*(?:\([^\\\x80-\xff\n\015()]*(?:(?:\\[^\x80-\xff]|\([^\\\x80-\xff\n\015()]*(?:\\[^\x80-\xff][^\\\x80-\xff\n\015()]*)*\))[^\\\x80-\xff\n\015()]*)*\)[\040\t]*)*(?:\.[\040\t]*(?:\([^\\\x80-\xff\n\015()]*(?:(?:\\[^\x80-\xff]|\([^\\\x80-\xff\n\015()]*(?:\\[^\x80-\xff][^\\\x80-\xff\n\015()]*)*\))[^\\\x80-\xff\n\015()]*)*\)[\040\t]*)*(?:[^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff]+(?![^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff])|\[(?:[^\\\x80-\xff\n\015\[\]]|\\[^\x80-\xff])*\])[\040\t]*(?:\([^\\\x80-\xff\n\015()]*(?:(?:\\[^\x80-\xff]|\([^\\\x80-\xff\n\015()]*(?:\\[^\x80-\xff][^\\\x80-\xff\n\015()]*)*\))[^\\\x80-\xff\n\015()]*)*\)[\040\t]*)*)*)*:[\040\t]*(?:\([^\\\x80-\xff\n\015()]*(?:(?:\\[^\x80-\xff]|\([^\\\x80-\xff\n\015()]*(?:\\[^\x80-\xff][^\\\x80-\xff\n\015()]*)*\))[^\\\x80-\xff\n\015()]*)*\)[\040\t]*)*)?(?:[^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff]+(?![^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff])|"[^\\\x80-\xff\n\015"]*(?:\\[^\x80-\xff][^\\\x80-\xff\n\015"]*)*")[\040\t]*(?:\([^\\\x80-\xff\n\015()]*(?:(?:\\[^\x80-\xff]|\([^\\\x80-\xff\n\015()]*(?:\\[^\x80-\xff][^\\\x80-\xff\n\015()]*)*\))[^\\\x80-\xff\n\015()]*)*\)[\040\t]*)*(?:\.[\040\t]*(?:\([^\\\x80-\xff\n\015()]*(?:(?:\\[^\x80-\xff]|\([^\\\x80-\xff\n\015()]*(?:\\[^\x80-\xff][^\\\x80-\xff\n\015()]*)*\))[^\\\x80-\xff\n\015()]*)*\)[\040\t]*)*(?:[^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff]+(?![^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff])|"[^\\\x80-\xff\n\015"]*(?:\\[^\x80-\xff][^\\\x80-\xff\n\015"]*)*")[\040\t]*(?:\([^\\\x80-\xff\n\015()]*(?:(?:\\[^\x80-\xff]|\([^\\\x80-\xff\n\015()]*(?:\\[^\x80-\xff][^\\\x80-\xff\n\015()]*)*\))[^\\\x80-\xff\n\015()]*)*\)[\040\t]*)*)*@[\040\t]*(?:\([^\\\x80-\xff\n\015()]*(?:(?:\\[^\x80-\xff]|\([^\\\x80-\xff\n\015()]*(?:\\[^\x80-\xff][^\\\x80-\xff\n\015()]*)*\))[^\\\x80-\xff\n\015()]*)*\)[\040\t]*)*(?:[^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff]+(?![^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff])|\[(?:[^\\\x80-\xff\n\015\[\]]|\\[^\x80-\xff])*\])[\040\t]*(?:\([^\\\x80-\xff\n\015()]*(?:(?:\\[^\x80-\xff]|\([^\\\x80-\xff\n\015()]*(?:\\[^\x80-\xff][^\\\x80-\xff\n\015()]*)*\))[^\\\x80-\xff\n\015()]*)*\)[\040\t]*)*(?:\.[\040\t]*(?:\([^\\\x80-\xff\n\015()]*(?:(?:\\[^\x80-\xff]|\([^\\\x80-\xff\n\015()]*(?:\\[^\x80-\xff][^\\\x80-\xff\n\015()]*)*\))[^\\\x80-\xff\n\015()]*)*\)[\040\t]*)*(?:[^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff]+(?![^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff])|\[(?:[^\\\x80-\xff\n\015\[\]]|\\[^\x80-\xff])*\])[\040\t]*(?:\([^\\\x80-\xff\n\015()]*(?:(?:\\[^\x80-\xff]|\([^\\\x80-\xff\n\015()]*(?:\\[^\x80-\xff][^\\\x80-\xff\n\015()]*)*\))[^\\\x80-\xff\n\015()]*)*\)[\040\t]*)*)*>)
    // Skipping Unicode-unfriendly [\040\t]*(?:\([^\\\x80-\xff\n\015()]*(?:(?:\\[^\x80-\xff]|\([^\\\x80-\xff\n\015()]*(?:\\[^\x80-\xff][^\\\x80-\xff\n\015()]*)*\))[^\\\x80-\xff\n\015()]*)*\)[\040\t]*)*(?:(?:[^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff]+(?![^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff])|"[^\\\x80-\xff\n\015"]*(?:\\[^\x80-\xff][^\\\x80-\xff\n\015"]*)*")[\040\t]*(?:\([^\\\x80-\xff\n\015()]*(?:(?:\\[^\x80-\xff]|\([^\\\x80-\xff\n\015()]*(?:\\[^\x80-\xff][^\\\x80-\xff\n\015()]*)*\))[^\\\x80-\xff\n\015()]*)*\)[\040\t]*)*(?:\.[\040\t]*(?:\([^\\\x80-\xff\n\015()]*(?:(?:\\[^\x80-\xff]|\([^\\\x80-\xff\n\015()]*(?:\\[^\x80-\xff][^\\\x80-\xff\n\015()]*)*\))[^\\\x80-\xff\n\015()]*)*\)[\040\t]*)*(?:[^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff]+(?![^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff])|"[^\\\x80-\xff\n\015"]*(?:\\[^\x80-\xff][^\\\x80-\xff\n\015"]*)*")[\040\t]*(?:\([^\\\x80-\xff\n\015()]*(?:(?:\\[^\x80-\xff]|\([^\\\x80-\xff\n\015()]*(?:\\[^\x80-\xff][^\\\x80-\xff\n\015()]*)*\))[^\\\x80-\xff\n\015()]*)*\)[\040\t]*)*)*@[\040\t]*(?:\([^\\\x80-\xff\n\015()]*(?:(?:\\[^\x80-\xff]|\([^\\\x80-\xff\n\015()]*(?:\\[^\x80-\xff][^\\\x80-\xff\n\015()]*)*\))[^\\\x80-\xff\n\015()]*)*\)[\040\t]*)*(?:[^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff]+(?![^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff])|\[(?:[^\\\x80-\xff\n\015\[\]]|\\[^\x80-\xff])*\])[\040\t]*(?:\([^\\\x80-\xff\n\015()]*(?:(?:\\[^\x80-\xff]|\([^\\\x80-\xff\n\015()]*(?:\\[^\x80-\xff][^\\\x80-\xff\n\015()]*)*\))[^\\\x80-\xff\n\015()]*)*\)[\040\t]*)*(?:\.[\040\t]*(?:\([^\\\x80-\xff\n\015()]*(?:(?:\\[^\x80-\xff]|\([^\\\x80-\xff\n\015()]*(?:\\[^\x80-\xff][^\\\x80-\xff\n\015()]*)*\))[^\\\x80-\xff\n\015()]*)*\)[\040\t]*)*(?:[^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff]+(?![^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff])|\[(?:[^\\\x80-\xff\n\015\[\]]|\\[^\x80-\xff])*\])[\040\t]*(?:\([^\\\x80-\xff\n\015()]*(?:(?:\\[^\x80-\xff]|\([^\\\x80-\xff\n\015()]*(?:\\[^\x80-\xff][^\\\x80-\xff\n\015()]*)*\))[^\\\x80-\xff\n\015()]*)*\)[\040\t]*)*)*|(?:[^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff]+(?![^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff])|"[^\\\x80-\xff\n\015"]*(?:\\[^\x80-\xff][^\\\x80-\xff\n\015"]*)*")[^()<>@,;:".\\\[\]\x80-\xff\000-\010\012-\037]*(?:(?:\([^\\\x80-\xff\n\015()]*(?:(?:\\[^\x80-\xff]|\([^\\\x80-\xff\n\015()]*(?:\\[^\x80-\xff][^\\\x80-\xff\n\015()]*)*\))[^\\\x80-\xff\n\015()]*)*\)|"[^\\\x80-\xff\n\015"]*(?:\\[^\x80-\xff][^\\\x80-\xff\n\015"]*)*")[^()<>@,;:".\\\[\]\x80-\xff\000-\010\012-\037]*)*<[\040\t]*(?:\([^\\\x80-\xff\n\015()]*(?:(?:\\[^\x80-\xff]|\([^\\\x80-\xff\n\015()]*(?:\\[^\x80-\xff][^\\\x80-\xff\n\015()]*)*\))[^\\\x80-\xff\n\015()]*)*\)[\040\t]*)*(?:@[\040\t]*(?:\([^\\\x80-\xff\n\015()]*(?:(?:\\[^\x80-\xff]|\([^\\\x80-\xff\n\015()]*(?:\\[^\x80-\xff][^\\\x80-\xff\n\015()]*)*\))[^\\\x80-\xff\n\015()]*)*\)[\040\t]*)*(?:[^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff]+(?![^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff])|\[(?:[^\\\x80-\xff\n\015\[\]]|\\[^\x80-\xff])*\])[\040\t]*(?:\([^\\\x80-\xff\n\015()]*(?:(?:\\[^\x80-\xff]|\([^\\\x80-\xff\n\015()]*(?:\\[^\x80-\xff][^\\\x80-\xff\n\015()]*)*\))[^\\\x80-\xff\n\015()]*)*\)[\040\t]*)*(?:\.[\040\t]*(?:\([^\\\x80-\xff\n\015()]*(?:(?:\\[^\x80-\xff]|\([^\\\x80-\xff\n\015()]*(?:\\[^\x80-\xff][^\\\x80-\xff\n\015()]*)*\))[^\\\x80-\xff\n\015()]*)*\)[\040\t]*)*(?:[^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff]+(?![^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff])|\[(?:[^\\\x80-\xff\n\015\[\]]|\\[^\x80-\xff])*\])[\040\t]*(?:\([^\\\x80-\xff\n\015()]*(?:(?:\\[^\x80-\xff]|\([^\\\x80-\xff\n\015()]*(?:\\[^\x80-\xff][^\\\x80-\xff\n\015()]*)*\))[^\\\x80-\xff\n\015()]*)*\)[\040\t]*)*)*(?:,[\040\t]*(?:\([^\\\x80-\xff\n\015()]*(?:(?:\\[^\x80-\xff]|\([^\\\x80-\xff\n\015()]*(?:\\[^\x80-\xff][^\\\x80-\xff\n\015()]*)*\))[^\\\x80-\xff\n\015()]*)*\)[\040\t]*)*@[\040\t]*(?:\([^\\\x80-\xff\n\015()]*(?:(?:\\[^\x80-\xff]|\([^\\\x80-\xff\n\015()]*(?:\\[^\x80-\xff][^\\\x80-\xff\n\015()]*)*\))[^\\\x80-\xff\n\015()]*)*\)[\040\t]*)*(?:[^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff]+(?![^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff])|\[(?:[^\\\x80-\xff\n\015\[\]]|\\[^\x80-\xff])*\])[\040\t]*(?:\([^\\\x80-\xff\n\015()]*(?:(?:\\[^\x80-\xff]|\([^\\\x80-\xff\n\015()]*(?:\\[^\x80-\xff][^\\\x80-\xff\n\015()]*)*\))[^\\\x80-\xff\n\015()]*)*\)[\040\t]*)*(?:\.[\040\t]*(?:\([^\\\x80-\xff\n\015()]*(?:(?:\\[^\x80-\xff]|\([^\\\x80-\xff\n\015()]*(?:\\[^\x80-\xff][^\\\x80-\xff\n\015()]*)*\))[^\\\x80-\xff\n\015()]*)*\)[\040\t]*)*(?:[^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff]+(?![^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff])|\[(?:[^\\\x80-\xff\n\015\[\]]|\\[^\x80-\xff])*\])[\040\t]*(?:\([^\\\x80-\xff\n\015()]*(?:(?:\\[^\x80-\xff]|\([^\\\x80-\xff\n\015()]*(?:\\[^\x80-\xff][^\\\x80-\xff\n\015()]*)*\))[^\\\x80-\xff\n\015()]*)*\)[\040\t]*)*)*)*:[\040\t]*(?:\([^\\\x80-\xff\n\015()]*(?:(?:\\[^\x80-\xff]|\([^\\\x80-\xff\n\015()]*(?:\\[^\x80-\xff][^\\\x80-\xff\n\015()]*)*\))[^\\\x80-\xff\n\015()]*)*\)[\040\t]*)*)?(?:[^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff]+(?![^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff])|"[^\\\x80-\xff\n\015"]*(?:\\[^\x80-\xff][^\\\x80-\xff\n\015"]*)*")[\040\t]*(?:\([^\\\x80-\xff\n\015()]*(?:(?:\\[^\x80-\xff]|\([^\\\x80-\xff\n\015()]*(?:\\[^\x80-\xff][^\\\x80-\xff\n\015()]*)*\))[^\\\x80-\xff\n\015()]*)*\)[\040\t]*)*(?:\.[\040\t]*(?:\([^\\\x80-\xff\n\015()]*(?:(?:\\[^\x80-\xff]|\([^\\\x80-\xff\n\015()]*(?:\\[^\x80-\xff][^\\\x80-\xff\n\015()]*)*\))[^\\\x80-\xff\n\015()]*)*\)[\040\t]*)*(?:[^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff]+(?![^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff])|"[^\\\x80-\xff\n\015"]*(?:\\[^\x80-\xff][^\\\x80-\xff\n\015"]*)*")[\040\t]*(?:\([^\\\x80-\xff\n\015()]*(?:(?:\\[^\x80-\xff]|\([^\\\x80-\xff\n\015()]*(?:\\[^\x80-\xff][^\\\x80-\xff\n\015()]*)*\))[^\\\x80-\xff\n\015()]*)*\)[\040\t]*)*)*@[\040\t]*(?:\([^\\\x80-\xff\n\015()]*(?:(?:\\[^\x80-\xff]|\([^\\\x80-\xff\n\015()]*(?:\\[^\x80-\xff][^\\\x80-\xff\n\015()]*)*\))[^\\\x80-\xff\n\015()]*)*\)[\040\t]*)*(?:[^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff]+(?![^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff])|\[(?:[^\\\x80-\xff\n\015\[\]]|\\[^\x80-\xff])*\])[\040\t]*(?:\([^\\\x80-\xff\n\015()]*(?:(?:\\[^\x80-\xff]|\([^\\\x80-\xff\n\015()]*(?:\\[^\x80-\xff][^\\\x80-\xff\n\015()]*)*\))[^\\\x80-\xff\n\015()]*)*\)[\040\t]*)*(?:\.[\040\t]*(?:\([^\\\x80-\xff\n\015()]*(?:(?:\\[^\x80-\xff]|\([^\\\x80-\xff\n\015()]*(?:\\[^\x80-\xff][^\\\x80-\xff\n\015()]*)*\))[^\\\x80-\xff\n\015()]*)*\)[\040\t]*)*(?:[^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff]+(?![^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff])|\[(?:[^\\\x80-\xff\n\015\[\]]|\\[^\x80-\xff])*\])[\040\t]*(?:\([^\\\x80-\xff\n\015()]*(?:(?:\\[^\x80-\xff]|\([^\\\x80-\xff\n\015()]*(?:\\[^\x80-\xff][^\\\x80-\xff\n\015()]*)*\))[^\\\x80-\xff\n\015()]*)*\)[\040\t]*)*)*>)
    // Skipping Unicode-unfriendly [\040\t]*(?:\([^\\\x80-\xff\n\015()]*(?:(?:\\[^\x80-\xff]|\([^\\\x80-\xff\n\015()]*(?:\\[^\x80-\xff][^\\\x80-\xff\n\015()]*)*\))[^\\\x80-\xff\n\015()]*)*\)[\040\t]*)*(?:(?:[^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff]+(?![^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff])|"[^\\\x80-\xff\n\015"]*(?:\\[^\x80-\xff][^\\\x80-\xff\n\015"]*)*")[\040\t]*(?:\([^\\\x80-\xff\n\015()]*(?:(?:\\[^\x80-\xff]|\([^\\\x80-\xff\n\015()]*(?:\\[^\x80-\xff][^\\\x80-\xff\n\015()]*)*\))[^\\\x80-\xff\n\015()]*)*\)[\040\t]*)*(?:\.[\040\t]*(?:\([^\\\x80-\xff\n\015()]*(?:(?:\\[^\x80-\xff]|\([^\\\x80-\xff\n\015()]*(?:\\[^\x80-\xff][^\\\x80-\xff\n\015()]*)*\))[^\\\x80-\xff\n\015()]*)*\)[\040\t]*)*(?:[^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff]+(?![^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff])|"[^\\\x80-\xff\n\015"]*(?:\\[^\x80-\xff][^\\\x80-\xff\n\015"]*)*")[\040\t]*(?:\([^\\\x80-\xff\n\015()]*(?:(?:\\[^\x80-\xff]|\([^\\\x80-\xff\n\015()]*(?:\\[^\x80-\xff][^\\\x80-\xff\n\015()]*)*\))[^\\\x80-\xff\n\015()]*)*\)[\040\t]*)*)*@[\040\t]*(?:\([^\\\x80-\xff\n\015()]*(?:(?:\\[^\x80-\xff]|\([^\\\x80-\xff\n\015()]*(?:\\[^\x80-\xff][^\\\x80-\xff\n\015()]*)*\))[^\\\x80-\xff\n\015()]*)*\)[\040\t]*)*(?:[^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff]+(?![^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff])|\[(?:[^\\\x80-\xff\n\015\[\]]|\\[^\x80-\xff])*\])[\040\t]*(?:\([^\\\x80-\xff\n\015()]*(?:(?:\\[^\x80-\xff]|\([^\\\x80-\xff\n\015()]*(?:\\[^\x80-\xff][^\\\x80-\xff\n\015()]*)*\))[^\\\x80-\xff\n\015()]*)*\)[\040\t]*)*(?:\.[\040\t]*(?:\([^\\\x80-\xff\n\015()]*(?:(?:\\[^\x80-\xff]|\([^\\\x80-\xff\n\015()]*(?:\\[^\x80-\xff][^\\\x80-\xff\n\015()]*)*\))[^\\\x80-\xff\n\015()]*)*\)[\040\t]*)*(?:[^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff]+(?![^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff])|\[(?:[^\\\x80-\xff\n\015\[\]]|\\[^\x80-\xff])*\])[\040\t]*(?:\([^\\\x80-\xff\n\015()]*(?:(?:\\[^\x80-\xff]|\([^\\\x80-\xff\n\015()]*(?:\\[^\x80-\xff][^\\\x80-\xff\n\015()]*)*\))[^\\\x80-\xff\n\015()]*)*\)[\040\t]*)*)*|(?:[^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff]+(?![^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff])|"[^\\\x80-\xff\n\015"]*(?:\\[^\x80-\xff][^\\\x80-\xff\n\015"]*)*")[^()<>@,;:".\\\[\]\x80-\xff\000-\010\012-\037]*(?:(?:\([^\\\x80-\xff\n\015()]*(?:(?:\\[^\x80-\xff]|\([^\\\x80-\xff\n\015()]*(?:\\[^\x80-\xff][^\\\x80-\xff\n\015()]*)*\))[^\\\x80-\xff\n\015()]*)*\)|"[^\\\x80-\xff\n\015"]*(?:\\[^\x80-\xff][^\\\x80-\xff\n\015"]*)*")[^()<>@,;:".\\\[\]\x80-\xff\000-\010\012-\037]*)*<[\040\t]*(?:\([^\\\x80-\xff\n\015()]*(?:(?:\\[^\x80-\xff]|\([^\\\x80-\xff\n\015()]*(?:\\[^\x80-\xff][^\\\x80-\xff\n\015()]*)*\))[^\\\x80-\xff\n\015()]*)*\)[\040\t]*)*(?:@[\040\t]*(?:\([^\\\x80-\xff\n\015()]*(?:(?:\\[^\x80-\xff]|\([^\\\x80-\xff\n\015()]*(?:\\[^\x80-\xff][^\\\x80-\xff\n\015()]*)*\))[^\\\x80-\xff\n\015()]*)*\)[\040\t]*)*(?:[^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff]+(?![^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff])|\[(?:[^\\\x80-\xff\n\015\[\]]|\\[^\x80-\xff])*\])[\040\t]*(?:\([^\\\x80-\xff\n\015()]*(?:(?:\\[^\x80-\xff]|\([^\\\x80-\xff\n\015()]*(?:\\[^\x80-\xff][^\\\x80-\xff\n\015()]*)*\))[^\\\x80-\xff\n\015()]*)*\)[\040\t]*)*(?:\.[\040\t]*(?:\([^\\\x80-\xff\n\015()]*(?:(?:\\[^\x80-\xff]|\([^\\\x80-\xff\n\015()]*(?:\\[^\x80-\xff][^\\\x80-\xff\n\015()]*)*\))[^\\\x80-\xff\n\015()]*)*\)[\040\t]*)*(?:[^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff]+(?![^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff])|\[(?:[^\\\x80-\xff\n\015\[\]]|\\[^\x80-\xff])*\])[\040\t]*(?:\([^\\\x80-\xff\n\015()]*(?:(?:\\[^\x80-\xff]|\([^\\\x80-\xff\n\015()]*(?:\\[^\x80-\xff][^\\\x80-\xff\n\015()]*)*\))[^\\\x80-\xff\n\015()]*)*\)[\040\t]*)*)*(?:,[\040\t]*(?:\([^\\\x80-\xff\n\015()]*(?:(?:\\[^\x80-\xff]|\([^\\\x80-\xff\n\015()]*(?:\\[^\x80-\xff][^\\\x80-\xff\n\015()]*)*\))[^\\\x80-\xff\n\015()]*)*\)[\040\t]*)*@[\040\t]*(?:\([^\\\x80-\xff\n\015()]*(?:(?:\\[^\x80-\xff]|\([^\\\x80-\xff\n\015()]*(?:\\[^\x80-\xff][^\\\x80-\xff\n\015()]*)*\))[^\\\x80-\xff\n\015()]*)*\)[\040\t]*)*(?:[^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff]+(?![^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff])|\[(?:[^\\\x80-\xff\n\015\[\]]|\\[^\x80-\xff])*\])[\040\t]*(?:\([^\\\x80-\xff\n\015()]*(?:(?:\\[^\x80-\xff]|\([^\\\x80-\xff\n\015()]*(?:\\[^\x80-\xff][^\\\x80-\xff\n\015()]*)*\))[^\\\x80-\xff\n\015()]*)*\)[\040\t]*)*(?:\.[\040\t]*(?:\([^\\\x80-\xff\n\015()]*(?:(?:\\[^\x80-\xff]|\([^\\\x80-\xff\n\015()]*(?:\\[^\x80-\xff][^\\\x80-\xff\n\015()]*)*\))[^\\\x80-\xff\n\015()]*)*\)[\040\t]*)*(?:[^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff]+(?![^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff])|\[(?:[^\\\x80-\xff\n\015\[\]]|\\[^\x80-\xff])*\])[\040\t]*(?:\([^\\\x80-\xff\n\015()]*(?:(?:\\[^\x80-\xff]|\([^\\\x80-\xff\n\015()]*(?:\\[^\x80-\xff][^\\\x80-\xff\n\015()]*)*\))[^\\\x80-\xff\n\015()]*)*\)[\040\t]*)*)*)*:[\040\t]*(?:\([^\\\x80-\xff\n\015()]*(?:(?:\\[^\x80-\xff]|\([^\\\x80-\xff\n\015()]*(?:\\[^\x80-\xff][^\\\x80-\xff\n\015()]*)*\))[^\\\x80-\xff\n\015()]*)*\)[\040\t]*)*)?(?:[^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff]+(?![^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff])|"[^\\\x80-\xff\n\015"]*(?:\\[^\x80-\xff][^\\\x80-\xff\n\015"]*)*")[\040\t]*(?:\([^\\\x80-\xff\n\015()]*(?:(?:\\[^\x80-\xff]|\([^\\\x80-\xff\n\015()]*(?:\\[^\x80-\xff][^\\\x80-\xff\n\015()]*)*\))[^\\\x80-\xff\n\015()]*)*\)[\040\t]*)*(?:\.[\040\t]*(?:\([^\\\x80-\xff\n\015()]*(?:(?:\\[^\x80-\xff]|\([^\\\x80-\xff\n\015()]*(?:\\[^\x80-\xff][^\\\x80-\xff\n\015()]*)*\))[^\\\x80-\xff\n\015()]*)*\)[\040\t]*)*(?:[^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff]+(?![^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff])|"[^\\\x80-\xff\n\015"]*(?:\\[^\x80-\xff][^\\\x80-\xff\n\015"]*)*")[\040\t]*(?:\([^\\\x80-\xff\n\015()]*(?:(?:\\[^\x80-\xff]|\([^\\\x80-\xff\n\015()]*(?:\\[^\x80-\xff][^\\\x80-\xff\n\015()]*)*\))[^\\\x80-\xff\n\015()]*)*\)[\040\t]*)*)*@[\040\t]*(?:\([^\\\x80-\xff\n\015()]*(?:(?:\\[^\x80-\xff]|\([^\\\x80-\xff\n\015()]*(?:\\[^\x80-\xff][^\\\x80-\xff\n\015()]*)*\))[^\\\x80-\xff\n\015()]*)*\)[\040\t]*)*(?:[^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff]+(?![^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff])|\[(?:[^\\\x80-\xff\n\015\[\]]|\\[^\x80-\xff])*\])[\040\t]*(?:\([^\\\x80-\xff\n\015()]*(?:(?:\\[^\x80-\xff]|\([^\\\x80-\xff\n\015()]*(?:\\[^\x80-\xff][^\\\x80-\xff\n\015()]*)*\))[^\\\x80-\xff\n\015()]*)*\)[\040\t]*)*(?:\.[\040\t]*(?:\([^\\\x80-\xff\n\015()]*(?:(?:\\[^\x80-\xff]|\([^\\\x80-\xff\n\015()]*(?:\\[^\x80-\xff][^\\\x80-\xff\n\015()]*)*\))[^\\\x80-\xff\n\015()]*)*\)[\040\t]*)*(?:[^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff]+(?![^(\040)<>@,;:".\\\[\]\000-\037\x80-\xff])|\[(?:[^\\\x80-\xff\n\015\[\]]|\\[^\x80-\xff])*\])[\040\t]*(?:\([^\\\x80-\xff\n\015()]*(?:(?:\\[^\x80-\xff]|\([^\\\x80-\xff\n\015()]*(?:\\[^\x80-\xff][^\\\x80-\xff\n\015()]*)*\))[^\\\x80-\xff\n\015()]*)*\)[\040\t]*)*)*>)
    test_eq(run1_match("abc\\x0def\\x00pqr\\x000xyz\\x0000AB", "", "abc\u{d}ef\u{0}pqr\u{0}0xyz\u{0}00AB"), "abc\u{d}ef\u{0}pqr\u{0}0xyz\u{0}00AB"); // "abc\\x0def\\x00pqr\\x000xyz\\x0000AB"
    test_eq(run1_match("abc\\x0def\\x00pqr\\x000xyz\\x0000AB", "", "abc456 abc\u{d}ef\u{0}pqr\u{0}0xyz\u{0}00ABCDE"), "abc\u{d}ef\u{0}pqr\u{0}0xyz\u{0}00AB"); // "abc\\x0def\\x00pqr\\x000xyz\\x0000AB"
    // Skipping Unicode-unfriendly ^[\000-\037]
    // Skipping Unicode-unfriendly ^[\000-\037]
    // Skipping Unicode-unfriendly ^[\000-\037]
    test_eq(run1_match("\\0*", "", "\u{0}\u{0}\u{0}\u{0}"), "\u{0}\u{0}\u{0}\u{0}");
    test_eq(run1_match("^\\s", "", " abc"), " ");
    test_eq(run1_match("^\\s", "", "\u{c}abc"), "\u{c}");
    test_eq(run1_match("^\\s", "", "\nabc"), "\n");
    test_eq(run1_match("^\\s", "", "\u{d}abc"), "\u{d}");
    test_eq(run1_match("^\\s", "", "\u{9}abc"), "\u{9}");
    test_eq(run1_match("^abc", "", "abc"), "abc");
    test_eq(run1_match("ab{1,3}bc", "", "abbbbc"), "abbbbc");
    test_eq(run1_match("ab{1,3}bc", "", "abbbc"), "abbbc");
    test_eq(run1_match("ab{1,3}bc", "", "abbc"), "abbc");
    test_eq(run1_match("([^.]*)\\.([^:]*):[T ]+(.*)", "", "track1.title:TBlah blah blah"), "track1.title:TBlah blah blah,track1,title,Blah blah blah");
    test_eq(run1_match("([^.]*)\\.([^:]*):[T ]+(.*)", "i", "track1.title:TBlah blah blah"), "track1.title:TBlah blah blah,track1,title,Blah blah blah");
    test_eq(run1_match("([^.]*)\\.([^:]*):[t ]+(.*)", "i", "track1.title:TBlah blah blah"), "track1.title:TBlah blah blah,track1,title,Blah blah blah");
    test_eq(run1_match("^[W-c]+$", "", "WXY_^abc"), "WXY_^abc");
    test_eq(run1_match("^[W-c]+$", "i", "WXY_^abc"), "WXY_^abc");
    test_eq(run1_match("^[W-c]+$", "i", "wxy_^ABC"), "wxy_^ABC");
    test_eq(run1_match("^[\\x3f-\\x5F]+$", "i", "WXY_^abc"), "WXY_^abc");
    test_eq(run1_match("^[\\x3f-\\x5F]+$", "i", "wxy_^ABC"), "wxy_^ABC");
    test_eq(run1_match("^abc$", "m", "abc"), "abc");
    test_eq(run1_match("^abc$", "m", "qqq\nabc"), "abc");
    test_eq(run1_match("^abc$", "m", "abc\nzzz"), "abc");
    test_eq(run1_match("^abc$", "m", "qqq\nabc\nzzz"), "abc");
    test_eq(run1_match("^abc$", "", "abc"), "abc");
    test_eq(run1_match("(?:b)|(?::+)", "", "b::c"), "b");
    test_eq(run1_match("(?:b)|(?::+)", "", "c::b"), "::");
    test_eq(run1_match("[-az]+", "", "az-"), "az-");
    test_eq(run1_match("[-az]+", "", "*** Failers"), "a");
    test_eq(run1_match("[az-]+", "", "za-"), "za-");
    test_eq(run1_match("[az-]+", "", "*** Failers"), "a");
    test_eq(run1_match("[a\\-z]+", "", "a-z"), "a-z");
    test_eq(run1_match("[a\\-z]+", "", "*** Failers"), "a");
    test_eq(run1_match("[a-z]+", "", "abcdxyz"), "abcdxyz");
    test_eq(run1_match("[\\d-]+", "", "12-34"), "12-34"); // "[\\d-]+"
    // Skipping Unicode-unfriendly [\d-z]+
    test_eq(run1_match("\\x5c", "", "\\\\"), "\\");
    test_eq(run1_match("\\x20Z", "", "the Zoo"), " Z"); // "\\x20Z"
    // Skipping Unicode-unfriendly ab{3cd
    // Skipping Unicode-unfriendly ab{3,cd
    // Skipping Unicode-unfriendly ab{3,4a}cd
    // Skipping Unicode-unfriendly {4,5a}bc
    test_eq(run1_match("abc$", "", "abc"), "abc"); // "abc$"
    // Skipping Unicode-unfriendly (abc)\123
    // Skipping Unicode-unfriendly (abc)\223
    // Skipping Unicode-unfriendly (abc)\323
    // Skipping Unicode-unfriendly (abc)\100
    // Skipping Unicode-unfriendly (abc)\100
    // Skipping Unicode-unfriendly (abc)\100
    // Skipping Unicode-unfriendly (abc)\100
    // Skipping Unicode-unfriendly (abc)\100
    // Skipping Unicode-unfriendly (abc)\100
    // Skipping Unicode-unfriendly (abc)\100
    // Skipping Unicode-unfriendly (abc)\100
    // Skipping Unicode-unfriendly (a)(b)(c)(d)(e)(f)(g)(h)(i)(j)(k)\12\123
    // Skipping Unicode-unfriendly ab\idef
    test_eq(run1_match("a{0}bc", "", "bc"), "bc");
    test_eq(run1_match("(a|(bc)){0,0}?xyz", "", "xyz"), "xyz,,"); // "(a|(bc)){0,0}?xyz"
    // Skipping Unicode-unfriendly abc[\10]de
    // Skipping Unicode-unfriendly abc[\1]de
    // Skipping Unicode-unfriendly (abc)[\1]de
    test_eq(run1_match("^([^a])([^\\b])([^c]*)([^d]{3,4})", "", "baNOTccccd"), "baNOTcccc,b,a,NOT,cccc");
    test_eq(run1_match("^([^a])([^\\b])([^c]*)([^d]{3,4})", "", "baNOTcccd"), "baNOTccc,b,a,NOT,ccc");
    test_eq(run1_match("^([^a])([^\\b])([^c]*)([^d]{3,4})", "", "baNOTccd"), "baNOTcc,b,a,NO,Tcc");
    test_eq(run1_match("^([^a])([^\\b])([^c]*)([^d]{3,4})", "", "bacccd"), "baccc,b,a,,ccc");
    test_eq(run1_match("^([^a])([^\\b])([^c]*)([^d]{3,4})", "", "*** Failers"), "*** Failers,*,*,* Fail,ers");
    test_eq(run1_match("[^a]", "", "Abc"), "A");
    test_eq(run1_match("[^a]", "i", "Abc "), "b");
    test_eq(run1_match("[^a]+", "", "AAAaAbc"), "AAA");
    test_eq(run1_match("[^a]+", "i", "AAAaAbc "), "bc ");
    test_eq(run1_match("[^a]+", "", "bbb\nccc"), "bbb\nccc");
    test_eq(run1_match("[^k]$", "", "abc"), "c");
    test_eq(run1_match("[^k]$", "", "*** Failers"), "s");
    test_eq(run1_match("[^k]$", "", "abk   "), " ");
    test_eq(run1_match("[^k]{2,3}$", "", "abc"), "abc");
    test_eq(run1_match("[^k]{2,3}$", "", "kbc"), "bc");
    test_eq(run1_match("[^k]{2,3}$", "", "kabc "), "bc ");
    test_eq(run1_match("[^k]{2,3}$", "", "*** Failers"), "ers"); // "[^k]{2,3}$"
    // Skipping Unicode-unfriendly ^\d{8,}\@.+[^k]$
    // Skipping Unicode-unfriendly ^\d{8,}\@.+[^k]$
    test_eq(run1_match("[^a]", "", "aaaabcd"), "b");
    test_eq(run1_match("[^a]", "", "aaAabcd "), "A");
    test_eq(run1_match("[^a]", "i", "aaaabcd"), "b");
    test_eq(run1_match("[^a]", "i", "aaAabcd "), "b");
    test_eq(run1_match("[^az]", "", "aaaabcd"), "b");
    test_eq(run1_match("[^az]", "", "aaAabcd "), "A");
    test_eq(run1_match("[^az]", "i", "aaaabcd"), "b");
    test_eq(run1_match("[^az]", "i", "aaAabcd "), "b");
    test_eq(run1_match("P[^*]TAIRE[^*]{1,6}?LL", "", "xxxxxxxxxxxPSTAIREISLLxxxxxxxxx"), "PSTAIREISLL");
    test_eq(run1_match("P[^*]TAIRE[^*]{1,}?LL", "", "xxxxxxxxxxxPSTAIREISLLxxxxxxxxx"), "PSTAIREISLL");
    test_eq(run1_match("(\\.\\d\\d[1-9]?)\\d+", "", "1.230003938"), ".230003938,.23");
    test_eq(run1_match("(\\.\\d\\d[1-9]?)\\d+", "", "1.875000282   "), ".875000282,.875");
    test_eq(run1_match("(\\.\\d\\d[1-9]?)\\d+", "", "1.235  "), ".235,.23");
    test_eq(run1_match("(\\.\\d\\d((?=0)|\\d(?=\\d)))", "", "1.230003938      "), ".23,.23,");
    test_eq(run1_match("(\\.\\d\\d((?=0)|\\d(?=\\d)))", "", "1.875000282"), ".875,.875,5");
    test_eq(run1_match("\\b(foo)\\s+(\\w+)", "i", "Food is on the foo table"), "foo table,foo,table");
    test_eq(run1_match("foo(.*)bar", "", "The food is under the bar in the barn."), "food is under the bar in the bar,d is under the bar in the "); // "foo(.*)bar"
    test_eq(run1_match("foo(.*?)bar", "", "The food is under the bar in the barn."), "food is under the bar,d is under the "); // "foo(.*?)bar"
    test_eq(run1_match("(.*)(\\d*)", "", "I have 2 numbers: 53147"), "I have 2 numbers: 53147,I have 2 numbers: 53147,");
    test_eq(run1_match("(.*)(\\d+)", "", "I have 2 numbers: 53147"), "I have 2 numbers: 53147,I have 2 numbers: 5314,7");
    test_eq(run1_match("(.*?)(\\d*)", "", "I have 2 numbers: 53147"), ",,");
    test_eq(run1_match("(.*?)(\\d+)", "", "I have 2 numbers: 53147"), "I have 2,I have ,2");
    test_eq(run1_match("(.*)(\\d+)$", "", "I have 2 numbers: 53147"), "I have 2 numbers: 53147,I have 2 numbers: 5314,7");
    test_eq(run1_match("(.*?)(\\d+)$", "", "I have 2 numbers: 53147"), "I have 2 numbers: 53147,I have 2 numbers: ,53147");
    test_eq(run1_match("(.*)\\b(\\d+)$", "", "I have 2 numbers: 53147"), "I have 2 numbers: 53147,I have 2 numbers: ,53147");
    test_eq(run1_match("(.*\\D)(\\d+)$", "", "I have 2 numbers: 53147"), "I have 2 numbers: 53147,I have 2 numbers: ,53147");
    test_eq(run1_match("^\\D*(?!123)", "", "ABC123"), "AB");
    test_eq(run1_match("^\\D*(?!123)", "", " "), " ");
    test_eq(run1_match("^(\\D*)(?=\\d)(?!123)", "", "ABC445"), "ABC,ABC"); // "^(\\D*)(?=\\d)(?!123)"
    // Skipping Unicode-unfriendly ^[W-]46]
    // Skipping Unicode-unfriendly ^[W-]46]
    test_eq(run1_match("^[W-\\]46]", "", "W46]789 "), "W");
    test_eq(run1_match("^[W-\\]46]", "", "Wall"), "W");
    test_eq(run1_match("^[W-\\]46]", "", "Zebra"), "Z");
    test_eq(run1_match("^[W-\\]46]", "", "Xylophone  "), "X");
    test_eq(run1_match("^[W-\\]46]", "", "42"), "4");
    test_eq(run1_match("^[W-\\]46]", "", "[abcd] "), "[");
    test_eq(run1_match("^[W-\\]46]", "", "]abcd["), "]");
    test_eq(run1_match("^[W-\\]46]", "", "\\backslash "), "\\");
    test_eq(run1_match("\\d\\d\\/\\d\\d\\/\\d\\d\\d\\d", "", "01/01/2000"), "01/01/2000");
    test_eq(run1_match("^(a){0,0}", "", "bcd"), ",");
    test_eq(run1_match("^(a){0,0}", "", "abc"), ",");
    test_eq(run1_match("^(a){0,0}", "", "aab     "), ",");
    test_eq(run1_match("^(a){0,1}", "", "bcd"), ",");
    test_eq(run1_match("^(a){0,1}", "", "abc"), "a,a");
    test_eq(run1_match("^(a){0,1}", "", "aab  "), "a,a");
    test_eq(run1_match("^(a){0,2}", "", "bcd"), ",");
    test_eq(run1_match("^(a){0,2}", "", "abc"), "a,a");
    test_eq(run1_match("^(a){0,2}", "", "aab  "), "aa,a");
    test_eq(run1_match("^(a){0,3}", "", "bcd"), ",");
    test_eq(run1_match("^(a){0,3}", "", "abc"), "a,a");
    test_eq(run1_match("^(a){0,3}", "", "aab"), "aa,a");
    test_eq(run1_match("^(a){0,3}", "", "aaa   "), "aaa,a");
    test_eq(run1_match("^(a){0,}", "", "bcd"), ",");
    test_eq(run1_match("^(a){0,}", "", "abc"), "a,a");
    test_eq(run1_match("^(a){0,}", "", "aab"), "aa,a");
    test_eq(run1_match("^(a){0,}", "", "aaa"), "aaa,a");
    test_eq(run1_match("^(a){0,}", "", "aaaaaaaa    "), "aaaaaaaa,a");
    test_eq(run1_match("^(a){1,1}", "", "abc"), "a,a");
    test_eq(run1_match("^(a){1,1}", "", "aab  "), "a,a");
    test_eq(run1_match("^(a){1,2}", "", "abc"), "a,a");
    test_eq(run1_match("^(a){1,2}", "", "aab  "), "aa,a");
    test_eq(run1_match("^(a){1,3}", "", "abc"), "a,a");
    test_eq(run1_match("^(a){1,3}", "", "aab"), "aa,a");
    test_eq(run1_match("^(a){1,3}", "", "aaa   "), "aaa,a");
    test_eq(run1_match("^(a){1,}", "", "abc"), "a,a");
    test_eq(run1_match("^(a){1,}", "", "aab"), "aa,a");
    test_eq(run1_match("^(a){1,}", "", "aaa"), "aaa,a");
    test_eq(run1_match("^(a){1,}", "", "aaaaaaaa    "), "aaaaaaaa,a");
    test_eq(run1_match(".*\\.gif", "", "borfle\nbib.gif\nno"), "bib.gif");
    test_eq(run1_match(".{0,}\\.gif", "", "borfle\nbib.gif\nno"), "bib.gif");
    test_eq(run1_match(".*\\.gif", "m", "borfle\nbib.gif\nno"), "bib.gif");
    test_eq(run1_match(".*\\.gif", "", "borfle\nbib.gif\nno"), "bib.gif");
    test_eq(run1_match(".*\\.gif", "m", "borfle\nbib.gif\nno"), "bib.gif");
    test_eq(run1_match(".*$", "", "borfle\nbib.gif\nno"), "no");
    test_eq(run1_match(".*$", "m", "borfle\nbib.gif\nno"), "borfle");
    test_eq(run1_match(".*$", "", "borfle\nbib.gif\nno"), "no");
    test_eq(run1_match(".*$", "m", "borfle\nbib.gif\nno"), "borfle");
    test_eq(run1_match(".*$", "", "borfle\nbib.gif\nno\n"), "");
    test_eq(run1_match(".*$", "m", "borfle\nbib.gif\nno\n"), "borfle");
    test_eq(run1_match(".*$", "", "borfle\nbib.gif\nno\n"), "");
    test_eq(run1_match(".*$", "m", "borfle\nbib.gif\nno\n"), "borfle");
    test_eq(run1_match("(.*X|^B)", "", "abcde\n1234Xyz"), "1234X,1234X");
    test_eq(run1_match("(.*X|^B)", "", "BarFoo "), "B,B");
    test_eq(run1_match("(.*X|^B)", "m", "abcde\n1234Xyz"), "1234X,1234X");
    test_eq(run1_match("(.*X|^B)", "m", "BarFoo "), "B,B");
    test_eq(run1_match("(.*X|^B)", "m", "abcde\nBar  "), "B,B");
    test_eq(run1_match("(.*X|^B)", "", "abcde\n1234Xyz"), "1234X,1234X");
    test_eq(run1_match("(.*X|^B)", "", "BarFoo "), "B,B");
    test_eq(run1_match("(.*X|^B)", "m", "abcde\n1234Xyz"), "1234X,1234X");
    test_eq(run1_match("(.*X|^B)", "m", "BarFoo "), "B,B");
    test_eq(run1_match("(.*X|^B)", "m", "abcde\nBar  "), "B,B");
    test_eq(run1_match("(.*X|^B)", "m", "abcde\n1234Xyz"), "1234X,1234X");
    test_eq(run1_match("(.*X|^B)", "m", "BarFoo "), "B,B");
    test_eq(run1_match("(.*X|^B)", "m", "abcde\nBar  "), "B,B");
    test_eq(run1_match("(.*X|^B)", "m", "abcde\n1234Xyz"), "1234X,1234X");
    test_eq(run1_match("(.*X|^B)", "m", "BarFoo "), "B,B");
    test_eq(run1_match("(.*X|^B)", "m", "abcde\nBar  "), "B,B");
    test_eq(run1_match("^.*B", "", "B\n"), "B");
    test_eq(run1_match("^[0-9][0-9][0-9][0-9][0-9][0-9][0-9][0-9][0-9][0-9][0-9][0-9]", "", "123456654321"), "123456654321");
    test_eq(run1_match("^\\d\\d\\d\\d\\d\\d\\d\\d\\d\\d\\d\\d", "", "123456654321 "), "123456654321");
    test_eq(run1_match("^[\\d][\\d][\\d][\\d][\\d][\\d][\\d][\\d][\\d][\\d][\\d][\\d]", "", "123456654321"), "123456654321");
    test_eq(run1_match("^[abc]{12}", "", "abcabcabcabc"), "abcabcabcabc");
    test_eq(run1_match("^[a-c]{12}", "", "abcabcabcabc"), "abcabcabcabc");
    test_eq(run1_match("^(a|b|c){12}", "", "abcabcabcabc "), "abcabcabcabc,c");
    test_eq(run1_match("^[abcdefghijklmnopqrstuvwxy0123456789]", "", "n"), "n");
    test_eq(run1_match("abcde{0,0}", "", "abcd"), "abcd");
    test_eq(run1_match("ab[cd]{0,0}e", "", "abe"), "abe");
    test_eq(run1_match("ab(c){0,0}d", "", "abd"), "abd,");
    test_eq(run1_match("a(b*)", "", "a"), "a,");
    test_eq(run1_match("a(b*)", "", "ab"), "ab,b");
    test_eq(run1_match("a(b*)", "", "abbbb"), "abbbb,bbbb");
    test_eq(run1_match("a(b*)", "", "*** Failers"), "a,");
    test_eq(run1_match("ab\\d{0}e", "", "abe"), "abe");
    test_eq(run1_match("\"([^\\\\\"]+|\\\\.)*\"", "", "the \"quick\" brown fox"), "\"quick\",quick");
    test_eq(run1_match("\"([^\\\\\"]+|\\\\.)*\"", "", "\"the \\\"quick\\\" brown fox\" "), "\"the \\\"quick\\\" brown fox\", brown fox");
    test_eq(run1_match(".*?", "", "abc"), "");
    test_eq(run1_match("\\b", "", "abc "), "");
    test_eq(tc.compile("\\b").run_global_match("abc "), ","); // "\\b"
    // Skipping global "\\b" with string "abc"
    test_eq(run1_match("a[^a]b", "", "acb"), "acb");
    test_eq(run1_match("a[^a]b", "", "a\nb"), "a\nb");
    test_eq(run1_match("a.b", "", "acb"), "acb");
    test_eq(run1_match("a[^a]b", "", "acb"), "acb");
    test_eq(run1_match("a[^a]b", "", "a\nb  "), "a\nb");
    test_eq(run1_match("a.b", "", "acb"), "acb");
    test_eq(run1_match("^(b+?|a){1,2}?c", "", "bac"), "bac,a");
    test_eq(run1_match("^(b+?|a){1,2}?c", "", "bbac"), "bbac,a");
    test_eq(run1_match("^(b+?|a){1,2}?c", "", "bbbac"), "bbbac,a");
    test_eq(run1_match("^(b+?|a){1,2}?c", "", "bbbbac"), "bbbbac,a");
    test_eq(run1_match("^(b+?|a){1,2}?c", "", "bbbbbac "), "bbbbbac,a");
    test_eq(run1_match("^(b+|a){1,2}?c", "", "bac"), "bac,a");
    test_eq(run1_match("^(b+|a){1,2}?c", "", "bbac"), "bbac,a");
    test_eq(run1_match("^(b+|a){1,2}?c", "", "bbbac"), "bbbac,a");
    test_eq(run1_match("^(b+|a){1,2}?c", "", "bbbbac"), "bbbbac,a");
    test_eq(run1_match("^(b+|a){1,2}?c", "", "bbbbbac "), "bbbbbac,a"); // "^(b+|a){1,2}?c"
    // Skipping Unicode-unfriendly (?!\A)x
    // Skipping Unicode-unfriendly (?!\A)x
    test_eq(run1_match("(A|B)*?CD", "", "CD "), "CD,");
    test_eq(run1_match("(A|B)*CD", "", "CD "), "CD,");
    test_eq(run1_match("(\\d+)(\\w)", "", "12345a"), "12345a,12345,a");
    test_eq(run1_match("(\\d+)(\\w)", "", "12345+ "), "12345,1234,5");
    test_eq(run1_match("(\\d+)(\\w)", "", "12345a"), "12345a,12345,a");
    test_eq(run1_match("(\\d+)(\\w)", "", "12345+ "), "12345,1234,5");
    test_eq(run1_match("(a+|b+|c+)*c", "", "aaabbbbccccd"), "aaabbbbcccc,ccc");
    test_eq(run1_match("(a+|b+|c+)*c", "", "((abc(ade)ufh()()x"), "abc,b");
    test_eq(run1_match("(a+|b+|c+)*c", "", "(abc)"), "abc,b");
    test_eq(run1_match("(a+|b+|c+)*c", "", "(abc(def)xyz)"), "abc,b");
    test_eq(run1_match("(a+|b+|c+)*c", "", "a bcd e"), "bc,b");
    test_eq(run1_match("(a+|b+|c+)*c", "", "a b cd e"), "c,");
    test_eq(run1_match("(a+|b+|c+)*c", "", "abcd e   "), "abc,b");
    test_eq(run1_match("(a+|b+|c+)*c", "", "a bcde "), "bc,b");
    test_eq(run1_match("(a+|b+|c+)*c", "", "a bcde f"), "bc,b");
    test_eq(run1_match("(a+|b+|c+)*c", "", "abcdef  "), "abc,b");
    test_eq(run1_match("(a+|b+|c+)*c", "", "abc"), "abc,b");
    test_eq(run1_match("(a+|b+|c+)*c", "", "aBc"), "c,");
    test_eq(run1_match("(a+|b+|c+)*c", "", "Abc"), "bc,b");
    test_eq(run1_match("(a+|b+|c+)*c", "", "ABc"), "c,");
    test_eq(run1_match("(a+|b+|c+)*c", "", "abc"), "abc,b");
    test_eq(run1_match("(a+|b+|c+)*c", "", "aBc"), "c,");
    test_eq(run1_match("(a+|b+|c+)*c", "", "aBc"), "c,");
    test_eq(run1_match("(a+|b+|c+)*c", "", "aBBc"), "c,");
    test_eq(run1_match("(a+|b+|c+)*c", "", "abcd"), "abc,b");
    test_eq(run1_match("(a+|b+|c+)*c", "", "abcD     "), "abc,b");
    test_eq(run1_match("(a+|b+|c+)*c", "", "abc"), "abc,b");
    test_eq(run1_match("(a+|b+|c+)*c", "", "aBbc"), "bc,b");
    test_eq(run1_match("(a+|b+|c+)*c", "", "aBBc "), "c,");
    test_eq(run1_match("(a+|b+|c+)*c", "", "Abc"), "bc,b");
    test_eq(run1_match("(a+|b+|c+)*c", "", "abc"), "abc,b");
    test_eq(run1_match("(a+|b+|c+)*c", "", "aBc"), "c,");
    test_eq(run1_match("(a+|b+|c+)*c", "", "abxxc"), "c,");
    test_eq(run1_match("(a+|b+|c+)*c", "", "aBxxc"), "c,");
    test_eq(run1_match("(a+|b+|c+)*c", "", "Abxxc"), "c,");
    test_eq(run1_match("(a+|b+|c+)*c", "", "ABxxc"), "c,");
    test_eq(run1_match("(a+|b+|c+)*c", "", "abc:"), "abc,b");
    test_eq(run1_match("(a+|b+|c+)*c", "", "abc:"), "abc,b");
    test_eq(run1_match("(a+|b+|c+)*c", "", "cat"), "c,");
    test_eq(run1_match("(a+|b+|c+)*c", "", "fcat"), "c,");
    test_eq(run1_match("(a+|b+|c+)*c", "", "focat   "), "c,");
    test_eq(run1_match("(a+|b+|c+)*c", "", "foocat  "), "c,");
    test_eq(run1_match("(a+|b+|c+)*c", "", "cat"), "c,");
    test_eq(run1_match("(a+|b+|c+)*c", "", "fcat"), "c,");
    test_eq(run1_match("(a+|b+|c+)*c", "", "focat   "), "c,");
    test_eq(run1_match("(a+|b+|c+)*c", "", "foocat  "), "c,");
    test_eq(run1_match("(abc|)+", "", "abc"), "abc,abc");
    test_eq(run1_match("(abc|)+", "", "abcabc"), "abcabc,abc");
    test_eq(run1_match("(abc|)+", "", "abcabcabc"), "abcabcabc,abc");
    test_eq(run1_match("(abc|)+", "", "xyz      "), ",");
    test_eq(run1_match("([a]*)*", "", "a"), "a,a");
    test_eq(run1_match("([a]*)*", "", "aaaaa "), "aaaaa,aaaaa");
    test_eq(run1_match("([ab]*)*", "", "a"), "a,a");
    test_eq(run1_match("([ab]*)*", "", "b"), "b,b");
    test_eq(run1_match("([ab]*)*", "", "ababab"), "ababab,ababab");
    test_eq(run1_match("([ab]*)*", "", "aaaabcde"), "aaaab,aaaab");
    test_eq(run1_match("([ab]*)*", "", "bbbb    "), "bbbb,bbbb");
    test_eq(run1_match("([^a]*)*", "", "b"), "b,b");
    test_eq(run1_match("([^a]*)*", "", "bbbb"), "bbbb,bbbb");
    test_eq(run1_match("([^a]*)*", "", "aaa   "), ",");
    test_eq(run1_match("([^ab]*)*", "", "cccc"), "cccc,cccc");
    test_eq(run1_match("([^ab]*)*", "", "abab  "), ",");
    test_eq(run1_match("([a]*?)*", "", "a"), "a,a");
    test_eq(run1_match("([a]*?)*", "", "aaaa "), "aaaa,a");
    test_eq(run1_match("([ab]*?)*", "", "a"), "a,a");
    test_eq(run1_match("([ab]*?)*", "", "b"), "b,b");
    test_eq(run1_match("([ab]*?)*", "", "abab"), "abab,b");
    test_eq(run1_match("([ab]*?)*", "", "baba   "), "baba,a");
    test_eq(run1_match("([^a]*?)*", "", "b"), "b,b");
    test_eq(run1_match("([^a]*?)*", "", "bbbb"), "bbbb,b");
    test_eq(run1_match("([^a]*?)*", "", "aaa   "), ",");
    test_eq(run1_match("([^ab]*?)*", "", "c"), "c,c");
    test_eq(run1_match("([^ab]*?)*", "", "cccc"), "cccc,c");
    test_eq(run1_match("([^ab]*?)*", "", "baba   "), ",");
    test_eq(run1_match("([^ab]*?)*", "", "a"), ",");
    test_eq(run1_match("([^ab]*?)*", "", "aaabcde "), ",");
    test_eq(run1_match("([^ab]*?)*", "", "aaaaa"), ",");
    test_eq(run1_match("([^ab]*?)*", "", "aabbaa "), ",");
    test_eq(run1_match("([^ab]*?)*", "", "aaaaa"), ",");
    test_eq(run1_match("([^ab]*?)*", "", "aabbaa "), ",");
    test_eq(run1_match("([^ab]*?)*", "", "12-sep-98"), "12-sep-98,8");
    test_eq(run1_match("([^ab]*?)*", "", "12-09-98"), "12-09-98,8");
    test_eq(run1_match("([^ab]*?)*", "", "*** Failers"), "*** F,F");
    test_eq(run1_match("([^ab]*?)*", "", "sep-12-98"), "sep-12-98,8");
    test_eq(run1_match("([^ab]*?)*", "", "    "), "    , ");
    test_eq(run1_match("([^ab]*?)*", "", "saturday"), "s,s");
    test_eq(run1_match("([^ab]*?)*", "", "sunday"), "sund,d");
    test_eq(run1_match("([^ab]*?)*", "", "Saturday"), "S,S");
    test_eq(run1_match("([^ab]*?)*", "", "Sunday"), "Sund,d");
    test_eq(run1_match("([^ab]*?)*", "", "SATURDAY"), "SATURDAY,Y");
    test_eq(run1_match("([^ab]*?)*", "", "SUNDAY"), "SUNDAY,Y");
    test_eq(run1_match("([^ab]*?)*", "", "SunDay"), "SunD,D");
    test_eq(run1_match("([^ab]*?)*", "", "abcx"), ",");
    test_eq(run1_match("([^ab]*?)*", "", "aBCx"), ",");
    test_eq(run1_match("([^ab]*?)*", "", "bbx"), ",");
    test_eq(run1_match("([^ab]*?)*", "", "BBx"), "BBx,x");
    test_eq(run1_match("([^ab]*?)*", "", "*** Failers"), "*** F,F");
    test_eq(run1_match("([^ab]*?)*", "", "abcX"), ",");
    test_eq(run1_match("([^ab]*?)*", "", "aBCX"), ",");
    test_eq(run1_match("([^ab]*?)*", "", "bbX"), ",");
    test_eq(run1_match("([^ab]*?)*", "", "BBX               "), "BBX               , ");
    test_eq(run1_match("([^ab]*?)*", "", "ac"), ",");
    test_eq(run1_match("([^ab]*?)*", "", "aC"), ",");
    test_eq(run1_match("([^ab]*?)*", "", "bD"), ",");
    test_eq(run1_match("([^ab]*?)*", "", "elephant"), "eleph,h");
    test_eq(run1_match("([^ab]*?)*", "", "Europe "), "Europe , ");
    test_eq(run1_match("([^ab]*?)*", "", "frog"), "frog,g");
    test_eq(run1_match("([^ab]*?)*", "", "France"), "Fr,r");
    test_eq(run1_match("([^ab]*?)*", "", "*** Failers"), "*** F,F");
    test_eq(run1_match("([^ab]*?)*", "", "Africa     "), "Afric,c");
    test_eq(run1_match("([^ab]*?)*", "", "ab"), ",");
    test_eq(run1_match("([^ab]*?)*", "", "aBd"), ",");
    test_eq(run1_match("([^ab]*?)*", "", "xy"), "xy,y");
    test_eq(run1_match("([^ab]*?)*", "", "xY"), "xY,Y");
    test_eq(run1_match("([^ab]*?)*", "", "zebra"), "ze,e");
    test_eq(run1_match("([^ab]*?)*", "", "Zambesi"), "Z,Z");
    test_eq(run1_match("([^ab]*?)*", "", "*** Failers"), "*** F,F");
    test_eq(run1_match("([^ab]*?)*", "", "aCD  "), ",");
    test_eq(run1_match("([^ab]*?)*", "", "XY  "), "XY  , ");
    test_eq(run1_match("([^ab]*?)*", "", "foo\nbar"), "foo\n,\n");
    test_eq(run1_match("([^ab]*?)*", "", "*** Failers"), "*** F,F");
    test_eq(run1_match("([^ab]*?)*", "", "bar"), ",");
    test_eq(run1_match("([^ab]*?)*", "", "baz\nbar   "), ",");
    test_eq(run1_match("([^ab]*?)*", "", "barbaz"), ",");
    test_eq(run1_match("([^ab]*?)*", "", "barbarbaz "), ",");
    test_eq(run1_match("([^ab]*?)*", "", "koobarbaz "), "koo,o");
    test_eq(run1_match("([^ab]*?)*", "", "*** Failers"), "*** F,F");
    test_eq(run1_match("([^ab]*?)*", "", "baz"), ",");
    test_eq(run1_match("([^ab]*?)*", "", "foobarbaz "), "foo,o");
    test_eq(run1_match("abc", "", "abc"), "abc");
    test_eq(run1_match("abc", "", "xabcy"), "abc");
    test_eq(run1_match("abc", "", "ababc"), "abc");
    test_eq(run1_match("ab*c", "", "abc"), "abc");
    test_eq(run1_match("ab*bc", "", "abc"), "abc");
    test_eq(run1_match("ab*bc", "", "abbc"), "abbc");
    test_eq(run1_match("ab*bc", "", "abbbbc"), "abbbbc");
    test_eq(run1_match(".{1}", "", "abbbbc"), "a");
    test_eq(run1_match(".{3,4}", "", "abbbbc"), "abbb");
    test_eq(run1_match("ab{0,}bc", "", "abbbbc"), "abbbbc");
    test_eq(run1_match("ab+bc", "", "abbc"), "abbc");
    test_eq(run1_match("ab+bc", "", "abbbbc"), "abbbbc");
    test_eq(run1_match("ab{1,}bc", "", "abbbbc"), "abbbbc");
    test_eq(run1_match("ab{1,3}bc", "", "abbbbc"), "abbbbc");
    test_eq(run1_match("ab{3,4}bc", "", "abbbbc"), "abbbbc");
    test_eq(run1_match("ab?bc", "", "abbc"), "abbc");
    test_eq(run1_match("ab?bc", "", "abc"), "abc");
    test_eq(run1_match("ab{0,1}bc", "", "abc"), "abc");
    test_eq(run1_match("ab?c", "", "abc"), "abc");
    test_eq(run1_match("ab{0,1}c", "", "abc"), "abc");
    test_eq(run1_match("^abc$", "", "abc"), "abc");
    test_eq(run1_match("^abc", "", "abcc"), "abc");
    test_eq(run1_match("abc$", "", "aabc"), "abc");
    test_eq(run1_match("abc$", "", "aabc"), "abc");
    test_eq(run1_match("^", "", "abc"), "");
    test_eq(run1_match("$", "", "abc"), "");
    test_eq(run1_match("a.c", "", "abc"), "abc");
    test_eq(run1_match("a.c", "", "axc"), "axc");
    test_eq(run1_match("a.*c", "", "axyzc"), "axyzc");
    test_eq(run1_match("a[bc]d", "", "abd"), "abd");
    test_eq(run1_match("a[b-d]e", "", "ace"), "ace");
    test_eq(run1_match("a[b-d]", "", "aac"), "ac");
    test_eq(run1_match("a[-b]", "", "a-"), "a-");
    test_eq(run1_match("a[b-]", "", "a-"), "a-"); // "a[b-]"
    // Skipping Unicode-unfriendly a]
    test_eq(run1_match("a[^bc]d", "", "aed"), "aed");
    test_eq(run1_match("a[^-b]c", "", "adc"), "adc");
    test_eq(run1_match("\\ba\\b", "", "a-"), "a");
    test_eq(run1_match("\\ba\\b", "", "-a"), "a");
    test_eq(run1_match("\\ba\\b", "", "-a-"), "a");
    test_eq(run1_match("\\Ba\\B", "", "*** Failers"), "a");
    test_eq(run1_match("\\By\\b", "", "xy"), "y");
    test_eq(run1_match("\\by\\B", "", "yz"), "y");
    test_eq(run1_match("\\By\\B", "", "xyz"), "y");
    test_eq(run1_match("\\w", "", "a"), "a");
    test_eq(run1_match("\\W", "", "-"), "-");
    test_eq(run1_match("\\W", "", "*** Failers"), "*");
    test_eq(run1_match("\\W", "", "-"), "-");
    test_eq(run1_match("a\\sb", "", "a b"), "a b");
    test_eq(run1_match("a\\Sb", "", "a-b"), "a-b");
    test_eq(run1_match("a\\Sb", "", "a-b"), "a-b");
    test_eq(run1_match("\\d", "", "1"), "1");
    test_eq(run1_match("\\D", "", "-"), "-");
    test_eq(run1_match("\\D", "", "*** Failers"), "*");
    test_eq(run1_match("\\D", "", "-"), "-");
    test_eq(run1_match("[\\w]", "", "a"), "a");
    test_eq(run1_match("[\\W]", "", "-"), "-");
    test_eq(run1_match("[\\W]", "", "*** Failers"), "*");
    test_eq(run1_match("[\\W]", "", "-"), "-");
    test_eq(run1_match("a[\\s]b", "", "a b"), "a b");
    test_eq(run1_match("a[\\S]b", "", "a-b"), "a-b");
    test_eq(run1_match("a[\\S]b", "", "a-b"), "a-b");
    test_eq(run1_match("[\\d]", "", "1"), "1");
    test_eq(run1_match("[\\D]", "", "-"), "-");
    test_eq(run1_match("[\\D]", "", "*** Failers"), "*");
    test_eq(run1_match("[\\D]", "", "-"), "-");
    test_eq(run1_match("ab|cd", "", "abc"), "ab");
    test_eq(run1_match("ab|cd", "", "abcd"), "ab");
    test_eq(run1_match("()ef", "", "def"), "ef,");
    test_eq(run1_match("a\\(b", "", "a(b"), "a(b");
    test_eq(run1_match("((a))", "", "abc"), "a,a,a");
    test_eq(run1_match("(a)b(c)", "", "abc"), "abc,a,c");
    test_eq(run1_match("a+b+c", "", "aabbabc"), "abc");
    test_eq(run1_match("a{1,}b{1,}c", "", "aabbabc"), "abc");
    test_eq(run1_match("a.+?c", "", "abcabc"), "abc");
    test_eq(run1_match("(a+|b)*", "", "ab"), "ab,b");
    test_eq(run1_match("(a+|b){0,}", "", "ab"), "ab,b");
    test_eq(run1_match("(a+|b)+", "", "ab"), "ab,b");
    test_eq(run1_match("(a+|b){1,}", "", "ab"), "ab,b");
    test_eq(run1_match("(a+|b)?", "", "ab"), "a,a");
    test_eq(run1_match("(a+|b){0,1}", "", "ab"), "a,a");
    test_eq(run1_match("[^ab]*", "", "cde"), "cde");
    test_eq(run1_match("([abc])*d", "", "abbbcd"), "abbbcd,c");
    test_eq(run1_match("([abc])*bcd", "", "abcd"), "abcd,a"); // "([abc])*bcd"
    test_eq(run1_match("a|b|c|d|e", "", "e"), "e");
    test_eq(run1_match("(a|b|c|d|e)f", "", "ef"), "ef,e");
    test_eq(run1_match("abcd*efg", "", "abcdefg"), "abcdefg"); // "abcd*efg"
    test_eq(run1_match("ab*", "", "xabyabbbz"), "ab");
    test_eq(run1_match("ab*", "", "xayabbbz"), "a");
    test_eq(run1_match("(ab|cd)e", "", "abcde"), "cde,cd");
    test_eq(run1_match("[abhgefdc]ij", "", "hij"), "hij");
    test_eq(run1_match("(abc|)ef", "", "abcdef"), "ef,");
    test_eq(run1_match("(a|b)c*d", "", "abcd"), "bcd,b");
    test_eq(run1_match("(ab|ab*)bc", "", "abc"), "abc,a");
    test_eq(run1_match("a([bc]*)c*", "", "abc"), "abc,bc");
    test_eq(run1_match("a([bc]*)(c*d)", "", "abcd"), "abcd,bc,d");
    test_eq(run1_match("a([bc]+)(c*d)", "", "abcd"), "abcd,bc,d");
    test_eq(run1_match("a([bc]*)(c+d)", "", "abcd"), "abcd,b,cd");
    test_eq(run1_match("a[bcd]*dcdcde", "", "adcdcde"), "adcdcde"); // "a[bcd]*dcdcde"
    test_eq(run1_match("(ab|a)b*c", "", "abc"), "abc,ab");
    test_eq(run1_match("((a)(b)c)(d)", "", "abcd"), "abcd,abc,a,b,d");
    test_eq(run1_match("[a-zA-Z_][a-zA-Z0-9_]*", "", "alpha"), "alpha");
    test_eq(run1_match("^a(bc+|b[eh])g|.h$", "", "abh"), "bh,");
    test_eq(run1_match("(bc+d$|ef*g.|h?i(j|k))", "", "effgz"), "effgz,effgz,");
    test_eq(run1_match("(bc+d$|ef*g.|h?i(j|k))", "", "ij"), "ij,ij,j");
    test_eq(run1_match("(bc+d$|ef*g.|h?i(j|k))", "", "reffgz"), "effgz,effgz,");
    test_eq(run1_match("((((((((((a))))))))))", "", "a"), "a,a,a,a,a,a,a,a,a,a,a");
    test_eq(run1_match("(((((((((a)))))))))", "", "a"), "a,a,a,a,a,a,a,a,a,a");
    test_eq(run1_match("multiple words", "", "multiple words, yeah"), "multiple words"); // "multiple words"
    test_eq(run1_match("(.*)c(.*)", "", "abcde"), "abcde,ab,de");
    test_eq(run1_match("\\((.*), (.*)\\)", "", "(a, b)"), "(a, b),a,b");
    test_eq(run1_match("abcd", "", "abcd"), "abcd");
    test_eq(run1_match("a(bc)d", "", "abcd"), "abcd,bc");
    test_eq(run1_match("a[-]?c", "", "ac"), "ac");
    test_eq(run1_match("abc", "i", "ABC"), "ABC");
    test_eq(run1_match("abc", "i", "XABCY"), "ABC");
    test_eq(run1_match("abc", "i", "ABABC"), "ABC");
    test_eq(run1_match("ab*c", "i", "ABC"), "ABC");
    test_eq(run1_match("ab*bc", "i", "ABC"), "ABC");
    test_eq(run1_match("ab*bc", "i", "ABBC"), "ABBC");
    test_eq(run1_match("ab*?bc", "i", "ABBBBC"), "ABBBBC");
    test_eq(run1_match("ab{0,}?bc", "i", "ABBBBC"), "ABBBBC");
    test_eq(run1_match("ab+?bc", "i", "ABBC"), "ABBC");
    test_eq(run1_match("ab+bc", "i", "ABBBBC"), "ABBBBC");
    test_eq(run1_match("ab{1,}?bc", "i", "ABBBBC"), "ABBBBC");
    test_eq(run1_match("ab{1,3}?bc", "i", "ABBBBC"), "ABBBBC");
    test_eq(run1_match("ab{3,4}?bc", "i", "ABBBBC"), "ABBBBC");
    test_eq(run1_match("ab??bc", "i", "ABBC"), "ABBC");
    test_eq(run1_match("ab??bc", "i", "ABC"), "ABC");
    test_eq(run1_match("ab{0,1}?bc", "i", "ABC"), "ABC");
    test_eq(run1_match("ab??c", "i", "ABC"), "ABC");
    test_eq(run1_match("ab{0,1}?c", "i", "ABC"), "ABC");
    test_eq(run1_match("^abc$", "i", "ABC"), "ABC");
    test_eq(run1_match("^abc", "i", "ABCC"), "ABC");
    test_eq(run1_match("abc$", "i", "AABC"), "ABC");
    test_eq(run1_match("^", "i", "ABC"), "");
    test_eq(run1_match("$", "i", "ABC"), "");
    test_eq(run1_match("a.c", "i", "ABC"), "ABC");
    test_eq(run1_match("a.c", "i", "AXC"), "AXC");
    test_eq(run1_match("a.*?c", "i", "AXYZC"), "AXYZC");
    test_eq(run1_match("a.*c", "i", "AABC"), "AABC");
    test_eq(run1_match("a[bc]d", "i", "ABD"), "ABD");
    test_eq(run1_match("a[b-d]e", "i", "ACE"), "ACE");
    test_eq(run1_match("a[b-d]", "i", "AAC"), "AC");
    test_eq(run1_match("a[-b]", "i", "A-"), "A-");
    test_eq(run1_match("a[b-]", "i", "A-"), "A-"); // "a[b-]"
    // Skipping Unicode-unfriendly a]
    test_eq(run1_match("a[^bc]d", "i", "AED"), "AED");
    test_eq(run1_match("a[^-b]c", "i", "ADC"), "ADC");
    test_eq(run1_match("ab|cd", "i", "ABC"), "AB");
    test_eq(run1_match("ab|cd", "i", "ABCD"), "AB");
    test_eq(run1_match("()ef", "i", "DEF"), "EF,");
    test_eq(run1_match("a\\(b", "i", "A(B"), "A(B");
    test_eq(run1_match("((a))", "i", "ABC"), "A,A,A");
    test_eq(run1_match("(a)b(c)", "i", "ABC"), "ABC,A,C");
    test_eq(run1_match("a+b+c", "i", "AABBABC"), "ABC");
    test_eq(run1_match("a{1,}b{1,}c", "i", "AABBABC"), "ABC");
    test_eq(run1_match("a.+?c", "i", "ABCABC"), "ABC");
    test_eq(run1_match("a.*?c", "i", "ABCABC"), "ABC");
    test_eq(run1_match("a.{0,5}?c", "i", "ABCABC"), "ABC");
    test_eq(run1_match("(a+|b)*", "i", "AB"), "AB,B");
    test_eq(run1_match("(a+|b){0,}", "i", "AB"), "AB,B");
    test_eq(run1_match("(a+|b)+", "i", "AB"), "AB,B");
    test_eq(run1_match("(a+|b){1,}", "i", "AB"), "AB,B");
    test_eq(run1_match("(a+|b)?", "i", "AB"), "A,A");
    test_eq(run1_match("(a+|b){0,1}", "i", "AB"), "A,A");
    test_eq(run1_match("(a+|b){0,1}?", "i", "AB"), ",");
    test_eq(run1_match("[^ab]*", "i", "CDE"), "CDE");
    test_eq(run1_match("([abc])*d", "i", "ABBBCD"), "ABBBCD,C");
    test_eq(run1_match("([abc])*bcd", "i", "ABCD"), "ABCD,A"); // "([abc])*bcd"
    test_eq(run1_match("a|b|c|d|e", "i", "E"), "E");
    test_eq(run1_match("(a|b|c|d|e)f", "i", "EF"), "EF,E");
    test_eq(run1_match("abcd*efg", "i", "ABCDEFG"), "ABCDEFG"); // "abcd*efg"
    test_eq(run1_match("ab*", "i", "XABYABBBZ"), "AB");
    test_eq(run1_match("ab*", "i", "XAYABBBZ"), "A");
    test_eq(run1_match("(ab|cd)e", "i", "ABCDE"), "CDE,CD");
    test_eq(run1_match("[abhgefdc]ij", "i", "HIJ"), "HIJ");
    test_eq(run1_match("(abc|)ef", "i", "ABCDEF"), "EF,");
    test_eq(run1_match("(a|b)c*d", "i", "ABCD"), "BCD,B");
    test_eq(run1_match("(ab|ab*)bc", "i", "ABC"), "ABC,A");
    test_eq(run1_match("a([bc]*)c*", "i", "ABC"), "ABC,BC");
    test_eq(run1_match("a([bc]*)(c*d)", "i", "ABCD"), "ABCD,BC,D");
    test_eq(run1_match("a([bc]+)(c*d)", "i", "ABCD"), "ABCD,BC,D");
    test_eq(run1_match("a([bc]*)(c+d)", "i", "ABCD"), "ABCD,B,CD");
    test_eq(run1_match("a[bcd]*dcdcde", "i", "ADCDCDE"), "ADCDCDE"); // "a[bcd]*dcdcde"
    test_eq(run1_match("(ab|a)b*c", "i", "ABC"), "ABC,AB");
    test_eq(run1_match("((a)(b)c)(d)", "i", "ABCD"), "ABCD,ABC,A,B,D");
    test_eq(run1_match("[a-zA-Z_][a-zA-Z0-9_]*", "i", "ALPHA"), "ALPHA");
    test_eq(run1_match("^a(bc+|b[eh])g|.h$", "i", "ABH"), "BH,");
    test_eq(run1_match("(bc+d$|ef*g.|h?i(j|k))", "i", "EFFGZ"), "EFFGZ,EFFGZ,");
    test_eq(run1_match("(bc+d$|ef*g.|h?i(j|k))", "i", "IJ"), "IJ,IJ,J");
    test_eq(run1_match("(bc+d$|ef*g.|h?i(j|k))", "i", "REFFGZ"), "EFFGZ,EFFGZ,");
    test_eq(run1_match("((((((((((a))))))))))", "i", "A"), "A,A,A,A,A,A,A,A,A,A,A");
    test_eq(run1_match("(((((((((a)))))))))", "i", "A"), "A,A,A,A,A,A,A,A,A,A");
    test_eq(run1_match("(?:(?:(?:(?:(?:(?:(?:(?:(?:(a))))))))))", "i", "A"), "A,A");
    test_eq(run1_match("(?:(?:(?:(?:(?:(?:(?:(?:(?:(a|b|c))))))))))", "i", "C"), "C,C");
    test_eq(run1_match("multiple words", "i", "MULTIPLE WORDS, YEAH"), "MULTIPLE WORDS"); // "multiple words"
    test_eq(run1_match("(.*)c(.*)", "i", "ABCDE"), "ABCDE,AB,DE");
    test_eq(run1_match("\\((.*), (.*)\\)", "i", "(A, B)"), "(A, B),A,B");
    test_eq(run1_match("abcd", "i", "ABCD"), "ABCD");
    test_eq(run1_match("a(bc)d", "i", "ABCD"), "ABCD,BC");
    test_eq(run1_match("a[-]?c", "i", "AC"), "AC");
    test_eq(run1_match("a(?!b).", "", "abad"), "ad");
    test_eq(run1_match("a(?=d).", "", "abad"), "ad");
    test_eq(run1_match("a(?=c|d).", "", "abad"), "ad");
    test_eq(run1_match("a(?:b|c|d)(.)", "", "ace"), "ace,e");
    test_eq(run1_match("a(?:b|c|d)*(.)", "", "ace"), "ace,e");
    test_eq(run1_match("a(?:b|c|d)+?(.)", "", "ace"), "ace,e");
    test_eq(run1_match("a(?:b|c|d)+?(.)", "", "acdbcdbe"), "acd,d");
    test_eq(run1_match("a(?:b|c|d)+(.)", "", "acdbcdbe"), "acdbcdbe,e");
    test_eq(run1_match("a(?:b|c|d){2}(.)", "", "acdbcdbe"), "acdb,b");
    test_eq(run1_match("a(?:b|c|d){4,5}(.)", "", "acdbcdbe"), "acdbcdb,b");
    test_eq(run1_match("a(?:b|c|d){4,5}?(.)", "", "acdbcdbe"), "acdbcd,d");
    test_eq(run1_match("((foo)|(bar))*", "", "foobar"), "foobar,bar,,bar"); // "((foo)|(bar))*"
    test_eq(run1_match("a(?:b|c|d){6,7}(.)", "", "acdbcdbe"), "acdbcdbe,e");
    test_eq(run1_match("a(?:b|c|d){6,7}?(.)", "", "acdbcdbe"), "acdbcdbe,e");
    test_eq(run1_match("a(?:b|c|d){5,6}(.)", "", "acdbcdbe"), "acdbcdbe,e");
    test_eq(run1_match("a(?:b|c|d){5,6}?(.)", "", "acdbcdbe"), "acdbcdb,b");
    test_eq(run1_match("a(?:b|c|d){5,7}(.)", "", "acdbcdbe"), "acdbcdbe,e");
    test_eq(run1_match("a(?:b|c|d){5,7}?(.)", "", "acdbcdbe"), "acdbcdb,b");
    test_eq(run1_match("a(?:b|(c|e){1,2}?|d)+?(.)", "", "ace"), "ace,c,e");
    test_eq(run1_match("^(.+)?B", "", "AB"), "AB,A");
    test_eq(run1_match("^([^a-z])|(\\^)$", "", "."), ".,.,");
    test_eq(run1_match("^[<>]&", "", "<&OUT"), "<&");
    test_eq(run1_match("(?:(f)(o)(o)|(b)(a)(r))*", "", "foobar"), "foobar,,,,b,a,r");
    test_eq(run1_match("(?:(f)(o)(o)|(b)(a)(r))*", "", "ab"), ",,,,,,");
    test_eq(run1_match("(?:(f)(o)(o)|(b)(a)(r))*", "", "*** Failers"), ",,,,,,");
    test_eq(run1_match("(?:(f)(o)(o)|(b)(a)(r))*", "", "cb"), ",,,,,,");
    test_eq(run1_match("(?:(f)(o)(o)|(b)(a)(r))*", "", "b"), ",,,,,,");
    test_eq(run1_match("(?:(f)(o)(o)|(b)(a)(r))*", "", "ab"), ",,,,,,");
    test_eq(run1_match("(?:(f)(o)(o)|(b)(a)(r))*", "", "b"), ",,,,,,");
    test_eq(run1_match("(?:(f)(o)(o)|(b)(a)(r))*", "", "b"), ",,,,,,");
    test_eq(run1_match("(?:..)*a", "", "aba"), "aba");
    test_eq(run1_match("(?:..)*?a", "", "aba"), "a");
    test_eq(run1_match("^(){3,5}", "", "abc"), ",");
    test_eq(run1_match("^(a+)*ax", "", "aax"), "aax,a");
    test_eq(run1_match("^((a|b)+)*ax", "", "aax"), "aax,a,a");
    test_eq(run1_match("^((a|bc)+)*ax", "", "aax"), "aax,a,a");
    test_eq(run1_match("(a|x)*ab", "", "cab"), "ab,");
    test_eq(run1_match("(a)*ab", "", "cab"), "ab,");
    test_eq(run1_match("(a)*ab", "", "ab"), "ab,");
    test_eq(run1_match("(a)*ab", "", "ab"), "ab,");
    test_eq(run1_match("(a)*ab", "", "ab"), "ab,");
    test_eq(run1_match("(a)*ab", "", "ab"), "ab,");
    test_eq(run1_match("(a)*ab", "", "ab"), "ab,");
    test_eq(run1_match("(a)*ab", "", "ab"), "ab,");
    test_eq(run1_match("(a)*ab", "", "ab"), "ab,");
    test_eq(run1_match("(a)*ab", "", "ab"), "ab,");
    test_eq(run1_match("(?:c|d)(?:)(?:a(?:)(?:b)(?:b(?:))(?:b(?:)(?:b)))", "", "cabbbb"), "cabbbb");
    test_eq(run1_match("(?:c|d)(?:)(?:aaaaaaaa(?:)(?:bbbbbbbb)(?:bbbbbbbb(?:))(?:bbbbbbbb(?:)(?:bbbbbbbb)))", "", "caaaaaaaabbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"), "caaaaaaaabbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"); // "(?:c|d)(?:)(?:aaaaaaaa(?:)(?:bbbbbbbb)(?:bbbbbbbb(?:))(?:bbbbbbbb(?:)(?:bbbbbbbb)))"
    test_eq(run1_match("foo\\w*\\d{4}baz", "", "foobar1234baz"), "foobar1234baz"); // "foo\\w*\\d{4}baz"
    test_eq(run1_match("x(~~)*(?:(?:F)?)?", "", "x~~"), "x~~,~~");
    test_eq(run1_match("^a{3}c", "", "aaac"), "aaac");
    test_eq(run1_match("^a{3}c", "", "aaac"), "aaac");
    test_eq(run1_match("(\\w+:)+", "", "one:"), "one:,one:");
    test_eq(run1_match("([\\w:]+::)?(\\w+)$", "", "abcd"), "abcd,,abcd");
    test_eq(run1_match("([\\w:]+::)?(\\w+)$", "", "xy:z:::abcd"), "xy:z:::abcd,xy:z:::,abcd");
    test_eq(run1_match("^[^bcd]*(c+)", "", "aexycd"), "aexyc,c");
    test_eq(run1_match("(a*)b+", "", "caab"), "aab,aa");
    test_eq(run1_match("([\\w:]+::)?(\\w+)$", "", "abcd"), "abcd,,abcd");
    test_eq(run1_match("([\\w:]+::)?(\\w+)$", "", "xy:z:::abcd"), "xy:z:::abcd,xy:z:::,abcd");
    test_eq(run1_match("([\\w:]+::)?(\\w+)$", "", "*** Failers"), "Failers,,Failers");
    test_eq(run1_match("^[^bcd]*(c+)", "", "aexycd"), "aexyc,c");
    test_eq(run1_match("([[:]+)", "", "a:[b]:"), ":[,:[");
    test_eq(run1_match("([[=]+)", "", "a=[b]="), "=[,=[");
    test_eq(run1_match("([[.]+)", "", "a.[b]."), ".[,.[");
    test_eq(run1_match("((Z)+|A)*", "", "ZABCDEFG"), "ZA,A,");
    test_eq(run1_match("(Z()|A)*", "", "ZABCDEFG"), "ZA,A,");
    test_eq(run1_match("(Z(())|A)*", "", "ZABCDEFG"), "ZA,A,,");
    test_eq(run1_match("(Z(())|A)*", "", "ZABCDEFG"), "ZA,A,,");
    test_eq(run1_match("(Z(())|A)*", "", "ZABCDEFG"), "ZA,A,,");
    test_eq(run1_match("a*", "", "abbab"), "a"); // "a*"
    // Skipping Unicode-unfriendly a*
    test_eq(run1_match("a*", "", "-things"), ""); // "a*"
    // Skipping global "a*" with string "0digit"
    // Skipping global "a*" with string "*** Failers"
    // Skipping global "a*" with string "bcdef    "
    // Skipping Unicode-unfriendly ^[\d-a]
    // Skipping Unicode-unfriendly ^[\d-a]
    // Skipping Unicode-unfriendly ^[\d-a]
    test_eq(run1_match("[\\s]+", "", "> \u{9}\n\u{c}\u{d}\u{b}<"), " \u{9}\n\u{c}\u{d}\u{b}");
    test_eq(run1_match("[\\s]+", "", " "), " ");
    test_eq(run1_match("\\s+", "", "> \u{9}\n\u{c}\u{d}\u{b}<"), " \u{9}\n\u{c}\u{d}\u{b}");
    test_eq(run1_match("\\s+", "", " "), " ");
    test_eq(run1_match("abc.", "", "abc1abc2xyzabc3 "), "abc1"); // "abc."
    // Skipping global "abc." with string "XabcY"
    // Skipping global "abc." with string "XabcY  "
    // Skipping global "abc." with string "abcE"
    // Skipping Unicode-unfriendly [\z\C]
    // Skipping Unicode-unfriendly [\z\C]
    // Skipping Unicode-unfriendly \M
    test_eq(run1_match("(a+)*b", "", "bbbbc"), "b,");
    test_eq(run1_match("(a+)*b", "", "abc"), "ab,a");
    test_eq(run1_match("(a+)*b", "", "bca"), "b,");
    test_eq(run1_match("(a+)*b", "", "abc"), "ab,a");
    test_eq(run1_match("(a+)*b", "", "bca"), "b,");
    test_eq(run1_match("(a+)*b", "", "abc"), "ab,a");
    test_eq(run1_match("(a+)*b", "", "abc"), "ab,a");
    test_eq(run1_match("line\\nbreak", "", "this is a line\nbreak"), "line\nbreak"); // "line\\nbreak"
    test_eq(run1_match("line\\nbreak", "", "line one\nthis is a line\nbreak in the second line "), "line\nbreak"); // "line\\nbreak"
    test_eq(run1_match("line\\nbreak", "", "this is a line\nbreak"), "line\nbreak"); // "line\\nbreak"
    test_eq(run1_match("line\\nbreak", "", "line one\nthis is a line\nbreak in the second line "), "line\nbreak"); // "line\\nbreak"
    test_eq(run1_match("line\\nbreak", "m", "this is a line\nbreak"), "line\nbreak"); // "line\\nbreak"
    test_eq(run1_match("line\\nbreak", "m", "line one\nthis is a line\nbreak in the second line "), "line\nbreak"); // "line\\nbreak"
    test_eq(run1_match("^", "m", "a\nb\nc\n"), ""); // "^"
    // Skipping global "^" with string " "
    // Skipping global "^" with string "A\nC\nC\n "
    // Skipping global "^" with string "AB"
    // Skipping global "^" with string "aB  "
    // Skipping global "^" with string "AB"
    // Skipping global "^" with string "aB  "
    // Skipping global "^" with string "AB"
    // Skipping global "^" with string "aB  "
    // Skipping global "^" with string "AB"
    // Skipping global "^" with string "aB  "
    test_eq(run1_match("Content-Type\\x3A[^\\r\\n]{6,}", "", "Content-Type:xxxxxyyy "), "Content-Type:xxxxxyyy "); // "Content-Type\\x3A[^\\r\\n]{6,}"
    test_eq(run1_match("Content-Type\\x3A[^\\r\\n]{6,}z", "", "Content-Type:xxxxxyyyz"), "Content-Type:xxxxxyyyz"); // "Content-Type\\x3A[^\\r\\n]{6,}z"
    test_eq(run1_match("Content-Type\\x3A[^a]{6,}", "", "Content-Type:xxxyyy "), "Content-Type:xxxyyy "); // "Content-Type\\x3A[^a]{6,}"
    test_eq(run1_match("Content-Type\\x3A[^a]{6,}z", "", "Content-Type:xxxyyyz"), "Content-Type:xxxyyyz"); // "Content-Type\\x3A[^a]{6,}z"
    test_eq(run1_match("^abc", "m", "xyz\nabc"), "abc");
    test_eq(run1_match("^abc", "m", "xyz\nabc<lf>"), "abc");
    test_eq(run1_match("^abc", "m", "xyz\u{d}\nabc<lf>"), "abc");
    test_eq(run1_match("^abc", "m", "xyz\u{d}abc<cr>"), "abc");
    test_eq(run1_match("^abc", "m", "xyz\u{d}\nabc<crlf>"), "abc");
    test_eq(run1_match("^abc", "m", "xyz\nabc<cr>"), "abc");
    test_eq(run1_match("^abc", "m", "xyz\u{d}\nabc<cr>"), "abc");
    test_eq(run1_match("^abc", "m", "xyz\nabc<crlf>"), "abc");
    test_eq(run1_match("^abc", "m", "xyz\u{d}abc<crlf>"), "abc");
    test_eq(run1_match("^abc", "m", "xyz\u{d}abc<lf>"), "abc");
    test_eq(run1_match("abc$", "m", "xyzabc"), "abc");
    test_eq(run1_match("abc$", "m", "xyzabc\n "), "abc");
    test_eq(run1_match("abc$", "m", "xyzabc\npqr "), "abc");
    test_eq(run1_match("abc$", "m", "xyzabc\u{d}<cr> "), "abc");
    test_eq(run1_match("abc$", "m", "xyzabc\u{d}pqr<cr> "), "abc");
    test_eq(run1_match("abc$", "m", "xyzabc\u{d}\n<crlf> "), "abc");
    test_eq(run1_match("abc$", "m", "xyzabc\u{d}\npqr<crlf> "), "abc");
    test_eq(run1_match("abc$", "m", "xyzabc\u{d} "), "abc");
    test_eq(run1_match("abc$", "m", "xyzabc\u{d}pqr "), "abc");
    test_eq(run1_match("abc$", "m", "xyzabc\u{d}\n "), "abc");
    test_eq(run1_match("abc$", "m", "xyzabc\u{d}\npqr "), "abc");
    test_eq(run1_match("^abc", "m", "xyz\u{d}abcdef"), "abc");
    test_eq(run1_match("^abc", "m", "xyz\nabcdef<lf>"), "abc");
    test_eq(run1_match("^abc", "m", "xyz\nabcdef"), "abc");
    test_eq(run1_match("^abc", "m", "xyz\nabcdef"), "abc");
    test_eq(run1_match("^abc", "m", "xyz\u{d}abcdef<cr>"), "abc");
    test_eq(run1_match("^abc", "m", "xyz\u{d}abcdef"), "abc");
    test_eq(run1_match("^abc", "m", "xyz\u{d}\nabcdef"), "abc");
    test_eq(run1_match("^abc", "m", "xyz\u{d}abcdef<cr>"), "abc");
    test_eq(run1_match("^abc", "m", "xyz\u{d}abcdef"), "abc");
    test_eq(run1_match(".*", "", "abc\ndef"), "abc");
    test_eq(run1_match(".*", "", "abc\u{d}def"), "abc");
    test_eq(run1_match(".*", "", "abc\u{d}\ndef"), "abc");
    test_eq(run1_match(".*", "", "<cr>abc\ndef"), "<cr>abc");
    test_eq(run1_match(".*", "", "<cr>abc\u{d}def"), "<cr>abc");
    test_eq(run1_match(".*", "", "<cr>abc\u{d}\ndef"), "<cr>abc");
    test_eq(run1_match(".*", "", "<crlf>abc\ndef"), "<crlf>abc");
    test_eq(run1_match(".*", "", "<crlf>abc\u{d}def"), "<crlf>abc");
    test_eq(run1_match(".*", "", "<crlf>abc\u{d}\ndef"), "<crlf>abc");
    test_eq(run1_match("^\\w+=.*(\\\\\\n.*)*", "", "abc=xyz\\\npqr"), "abc=xyz\\,");
    test_eq(run1_match("^(a()*)*", "", "aaaa"), "aaaa,a,");
    test_eq(run1_match("^(?:a(?:(?:))*)*", "", "aaaa"), "aaaa");
    test_eq(run1_match("^(a()+)+", "", "aaaa"), "aaaa,a,");
    test_eq(run1_match("^(?:a(?:(?:))+)+", "", "aaaa"), "aaaa");
    test_eq(run1_match("^abc.", "m", "abc1 \nabc2 \u{b}abc3xx \u{c}abc4 \u{d}abc5xx \u{d}\nabc6 \u{85}abc7 JUNK"), "abc1");
    test_eq(run1_match("abc.$", "m", "abc1\n abc2\u{b} abc3\u{c} abc4\u{d} abc5\u{d}\n abc6\u{85} abc9"), "abc1"); // "abc.$"
    // Skipping Unicode-unfriendly ^a\R*b
    // Skipping Unicode-unfriendly ^a[\R]b
    test_eq(run1_match(".+foo", "", "afoo"), "afoo");
    test_eq(run1_match(".+foo", "", "afoo"), "afoo");
    test_eq(run1_match(".+foo", "", "afoo"), "afoo");
    test_eq(run1_match(".+foo", "", "afoo"), "afoo");
    test_eq(run1_match("^$", "m", "abc\u{d}\u{d}xyz"), ""); // "^$"
    // Skipping global "^$" with string "abc\n\u{d}xyz  "
    // Skipping global "^$" with string "abc\u{d}\nxyz"
    test_eq(run1_match("^X", "m", "XABC"), "X");
    test_eq(run1_match("^X", "m", "XABCB"), "X");
    test_eq(run1_match("\\nA", "", "\u{d}\nA "), "\nA");
    test_eq(run1_match("[\\r\\n]A", "", "\u{d}\nA "), "\nA");
    test_eq(run1_match("(\\r|\\n)A", "", "\u{d}\nA "), "\nA,\n");
    test_eq(run1_match("a(?!)|\\wbc", "", "abc "), "abc");
    test_eq(run1_match("a[^]b", "", "aXb"), "aXb");
    test_eq(run1_match("a[^]b", "", "a\nb "), "a\nb");
    test_eq(run1_match("a[^]+b", "", "aXb"), "aXb");
    test_eq(run1_match("a[^]+b", "", "a\nX\nXb "), "a\nX\nXb");
    test_eq(run1_match("\\bX", "", "Xoanon"), "X");
    test_eq(run1_match("\\bX", "", "+Xoanon"), "X");
    test_eq(run1_match("\\bX", "", "x{300}Xoanon "), "X");
    test_eq(run1_match("\\BX", "", "YXoanon"), "X");
    test_eq(run1_match("X\\b", "", "X+oanon"), "X");
    test_eq(run1_match("X\\b", "", "FAX "), "X");
    test_eq(run1_match("X\\B", "", "Xoanon  "), "X");
    test_eq(run1_match("X\\B", "", "ZXx{300}oanon "), "X");
    test_eq(run1_match("[^a]", "", "abcd"), "b");
    test_eq(run1_match("[^a]", "", "ax{100}   "), "x");
    test_eq(run1_match("[^a]", "", "ab99"), "b");
    test_eq(run1_match("[^a]", "", "x{123}x{123}45"), "x");
    test_eq(run1_match("[^a]", "", "x{400}x{401}x{402}6  "), "x");
    test_eq(run1_match("[^a]", "", "*** Failers"), "*");
    test_eq(run1_match("[^a]", "", "d99"), "d");
    test_eq(run1_match("[^a]", "", "x{123}x{122}4   "), "x");
    test_eq(run1_match("[^a]", "", "x{400}x{403}6  "), "x");
    test_eq(run1_match("[^a]", "", "x{400}x{401}x{402}x{402}6  "), "x");
    test_eq(run1_match("a.b", "", "acb"), "acb");
    test_eq(run1_match("a.b", "", "a\u{7f}b"), "a\u{7f}b");
    test_eq(run1_match("a(.*?)(.)", "", "a\u{c0}\u{88}b"), "a\u{c0},,\u{c0}");
    test_eq(run1_match("a(.*?)(.)", "", "ax{100}b"), "ax,,x");
    test_eq(run1_match("a(.*)(.)", "", "a\u{c0}\u{88}b"), "a\u{c0}\u{88}b,\u{c0}\u{88},b");
    test_eq(run1_match("a(.*)(.)", "", "ax{100}b"), "ax{100}b,x{100},b");
    test_eq(run1_match("a(.)(.)", "", "a\u{c0}\u{92}bcd"), "a\u{c0}\u{92},\u{c0},\u{92}");
    test_eq(run1_match("a(.)(.)", "", "ax{240}bcd"), "ax{,x,{");
    test_eq(run1_match("a(.?)(.)", "", "a\u{c0}\u{92}bcd"), "a\u{c0}\u{92},\u{c0},\u{92}");
    test_eq(run1_match("a(.?)(.)", "", "ax{240}bcd"), "ax{,x,{");
    test_eq(run1_match("a(.??)(.)", "", "a\u{c0}\u{92}bcd"), "a\u{c0},,\u{c0}");
    test_eq(run1_match("a(.??)(.)", "", "ax{240}bcd"), "ax,,x");
    test_eq(run1_match("a(.{3,})b", "", "ax{1234}xyb "), "ax{1234}xyb,x{1234}xy");
    test_eq(run1_match("a(.{3,})b", "", "ax{1234}x{4321}yb "), "ax{1234}x{4321}yb,x{1234}x{4321}y");
    test_eq(run1_match("a(.{3,})b", "", "ax{1234}x{4321}x{3412}b "), "ax{1234}x{4321}x{3412}b,x{1234}x{4321}x{3412}");
    test_eq(run1_match("a(.{3,})b", "", "axxxxbcdefghijb "), "axxxxbcdefghijb,xxxxbcdefghij");
    test_eq(run1_match("a(.{3,})b", "", "ax{1234}x{4321}x{3412}x{3421}b "), "ax{1234}x{4321}x{3412}x{3421}b,x{1234}x{4321}x{3412}x{3421}");
    test_eq(run1_match("a(.{3,})b", "", "ax{1234}b "), "ax{1234}b,x{1234}");
    test_eq(run1_match("a(.{3,}?)b", "", "ax{1234}xyb "), "ax{1234}xyb,x{1234}xy");
    test_eq(run1_match("a(.{3,}?)b", "", "ax{1234}x{4321}yb "), "ax{1234}x{4321}yb,x{1234}x{4321}y");
    test_eq(run1_match("a(.{3,}?)b", "", "ax{1234}x{4321}x{3412}b "), "ax{1234}x{4321}x{3412}b,x{1234}x{4321}x{3412}");
    test_eq(run1_match("a(.{3,}?)b", "", "axxxxbcdefghijb "), "axxxxb,xxxx");
    test_eq(run1_match("a(.{3,}?)b", "", "ax{1234}x{4321}x{3412}x{3421}b "), "ax{1234}x{4321}x{3412}x{3421}b,x{1234}x{4321}x{3412}x{3421}");
    test_eq(run1_match("a(.{3,}?)b", "", "ax{1234}b "), "ax{1234}b,x{1234}");
    test_eq(run1_match("a(.{3,5})b", "", "axxxxbcdefghijb "), "axxxxb,xxxx");
    test_eq(run1_match("a(.{3,5})b", "", "axbxxbcdefghijb "), "axbxxb,xbxx");
    test_eq(run1_match("a(.{3,5})b", "", "axxxxxbcdefghijb "), "axxxxxb,xxxxx");
    test_eq(run1_match("a(.{3,5}?)b", "", "axxxxbcdefghijb "), "axxxxb,xxxx");
    test_eq(run1_match("a(.{3,5}?)b", "", "axbxxbcdefghijb "), "axbxxb,xbxx");
    test_eq(run1_match("a(.{3,5}?)b", "", "axxxxxbcdefghijb "), "axxxxxb,xxxxx");
    test_eq(run1_match("[^a]+", "", "bcd"), "bcd"); // "[^a]+"
    // Skipping Unicode-unfriendly [^a]+
    test_eq(run1_match("^[^a]{2}", "", "x{100}bc"), "x{");
    test_eq(run1_match("^[^a]{2,}", "", "x{100}bcAa"), "x{100}bcA");
    test_eq(run1_match("^[^a]{2,}?", "", "x{100}bca"), "x{");
    test_eq(run1_match("[^a]+", "i", "bcd"), "bcd"); // "[^a]+"
    // Skipping Unicode-unfriendly [^a]+
    test_eq(run1_match("^[^a]{2}", "i", "x{100}bc"), "x{");
    test_eq(run1_match("^[^a]{2,}", "i", "x{100}bcAa"), "x{100}bc");
    test_eq(run1_match("^[^a]{2,}?", "i", "x{100}bca"), "x{");
    test_eq(run1_match("^[^a]{2,}?", "i", "x{100}x{100} "), "x{");
    test_eq(run1_match("^[^a]{2,}?", "i", "x{100}x{100} "), "x{");
    test_eq(run1_match("^[^a]{2,}?", "i", "x{100}x{100}x{100}x{100} "), "x{");
    test_eq(run1_match("^[^a]{2,}?", "i", "x{100}x{100}x{100}x{100} "), "x{");
    test_eq(run1_match("^[^a]{2,}?", "i", "Xyyyax{100}x{100}bXzzz"), "Xy");
    test_eq(run1_match("\\D", "", "1X2"), "X");
    test_eq(run1_match("\\D", "", "1x{100}2 "), "x");
    test_eq(run1_match(">\\S", "", "> >X Y"), ">X");
    test_eq(run1_match(">\\S", "", "> >x{100} Y"), ">x");
    test_eq(run1_match("\\d", "", "x{100}3"), "1");
    test_eq(run1_match("\\s", "", "x{100} X"), " ");
    test_eq(run1_match("\\D+", "", "12abcd34"), "abcd");
    test_eq(run1_match("\\D+", "", "*** Failers"), "*** Failers");
    test_eq(run1_match("\\D+", "", "1234  "), "  ");
    test_eq(run1_match("\\D{2,3}", "", "12abcd34"), "abc");
    test_eq(run1_match("\\D{2,3}", "", "12ab34"), "ab");
    test_eq(run1_match("\\D{2,3}", "", "*** Failers  "), "***");
    test_eq(run1_match("\\D{2,3}", "", "12a34  "), "  ");
    test_eq(run1_match("\\D{2,3}?", "", "12abcd34"), "ab");
    test_eq(run1_match("\\D{2,3}?", "", "12ab34"), "ab");
    test_eq(run1_match("\\D{2,3}?", "", "*** Failers  "), "**");
    test_eq(run1_match("\\D{2,3}?", "", "12a34  "), "  ");
    test_eq(run1_match("\\d+", "", "12abcd34"), "12");
    test_eq(run1_match("\\d{2,3}", "", "12abcd34"), "12");
    test_eq(run1_match("\\d{2,3}", "", "1234abcd"), "123");
    test_eq(run1_match("\\d{2,3}?", "", "12abcd34"), "12");
    test_eq(run1_match("\\d{2,3}?", "", "1234abcd"), "12");
    test_eq(run1_match("\\S+", "", "12abcd34"), "12abcd34");
    test_eq(run1_match("\\S+", "", "*** Failers"), "***");
    test_eq(run1_match("\\S{2,3}", "", "12abcd34"), "12a");
    test_eq(run1_match("\\S{2,3}", "", "1234abcd"), "123");
    test_eq(run1_match("\\S{2,3}", "", "*** Failers"), "***");
    test_eq(run1_match("\\S{2,3}?", "", "12abcd34"), "12");
    test_eq(run1_match("\\S{2,3}?", "", "1234abcd"), "12");
    test_eq(run1_match("\\S{2,3}?", "", "*** Failers"), "**");
    test_eq(run1_match(">\\s+<", "", "12>      <34"), ">      <");
    test_eq(run1_match(">\\s{2,3}<", "", "ab>  <cd"), ">  <");
    test_eq(run1_match(">\\s{2,3}<", "", "ab>   <ce"), ">   <");
    test_eq(run1_match(">\\s{2,3}?<", "", "ab>  <cd"), ">  <");
    test_eq(run1_match(">\\s{2,3}?<", "", "ab>   <ce"), ">   <");
    test_eq(run1_match("\\w+", "", "12      34"), "12");
    test_eq(run1_match("\\w+", "", "*** Failers"), "Failers");
    test_eq(run1_match("\\w{2,3}", "", "ab  cd"), "ab");
    test_eq(run1_match("\\w{2,3}", "", "abcd ce"), "abc");
    test_eq(run1_match("\\w{2,3}", "", "*** Failers"), "Fai");
    test_eq(run1_match("\\w{2,3}?", "", "ab  cd"), "ab");
    test_eq(run1_match("\\w{2,3}?", "", "abcd ce"), "ab");
    test_eq(run1_match("\\w{2,3}?", "", "*** Failers"), "Fa");
    test_eq(run1_match("\\W+", "", "12====34"), "====");
    test_eq(run1_match("\\W+", "", "*** Failers"), "*** ");
    test_eq(run1_match("\\W+", "", "abcd "), " ");
    test_eq(run1_match("\\W{2,3}", "", "ab====cd"), "===");
    test_eq(run1_match("\\W{2,3}", "", "ab==cd"), "==");
    test_eq(run1_match("\\W{2,3}", "", "*** Failers"), "***");
    test_eq(run1_match("\\W{2,3}?", "", "ab====cd"), "==");
    test_eq(run1_match("\\W{2,3}?", "", "ab==cd"), "==");
    test_eq(run1_match("\\W{2,3}?", "", "*** Failers"), "**");
    test_eq(run1_match("\\W{2,3}?", "", "*** Failers "), "**");
    test_eq(run1_match("\\W{2,3}?", "", "*** Failers "), "**");
    test_eq(run1_match("\\W{2,3}?", "", "*** Failers"), "**");
    test_eq(run1_match("\\W{2,3}?", "", "X  "), "  ");
    test_eq(run1_match("\\W{2,3}?", "", "*** Failers"), "**");
    test_eq(run1_match("\\W{2,3}?", "", "X  "), "  ");
    test_eq(run1_match("\\W{2,3}?", "", "*** Failers"), "**");
    test_eq(run1_match("\\W{2,3}?", "", "X  "), "  ");
    test_eq(run1_match("\\W{2,3}?", "", "*** Failers"), "**");
    test_eq(run1_match("\\W{2,3}?", "", "x{200}X   "), "  ");
    test_eq(run1_match("\\W{2,3}?", "", "*** Failers"), "**");
    test_eq(run1_match("\\W{2,3}?", "", "x{200}X   "), "  ");
    test_eq(run1_match("\\W{2,3}?", "", "*** Failers"), "**");
    test_eq(run1_match("\\W{2,3}?", "", "x{200}X   "), "  ");
    test_eq(run1_match("\\W{2,3}?", "", "*** Failers"), "**");
    test_eq(run1_match("\\W{2,3}?", "", "y    "), "  ");
    test_eq(run1_match("[\\xFF]", "", ">\u{ff}<"), "\u{ff}");
    test_eq(run1_match("[^\\xFF]", "", "XYZ"), "X");
    test_eq(run1_match("[^\\xff]", "", "XYZ"), "X");
    test_eq(run1_match("[^\\xff]", "", "x{123} "), "x");
    test_eq(run1_match("(|a)", "", "catac"), ","); // "(|a)"
    // Skipping global "(|a)" with string "ax{256}a "
    // Skipping global "(|a)" with string "x{85}"
    test_eq(run1_match("^abc.", "m", "abc1 \nabc2 \u{b}abc3xx \u{c}abc4 \u{d}abc5xx \u{d}\nabc6 x{0085}abc7 x{2028}abc8 x{2029}abc9 JUNK"), "abc1");
    test_eq(run1_match("abc.$", "m", "abc1\n abc2\u{b} abc3\u{c} abc4\u{d} abc5\u{d}\n abc6x{0085} abc7x{2028} abc8x{2029} abc9"), "abc1"); // "abc.$"
    // Skipping Unicode-unfriendly ^a\R*b
    test_eq(run1_match("X", "", "Ax{1ec5}ABCXYZ"), "X"); // "X"
    // Skipping Unicode-unfriendly \X?abc
    // Skipping Unicode-unfriendly \X?abc
    // Skipping Unicode-unfriendly \X?abc
    // Skipping Unicode-unfriendly \X?abc
    // Skipping Unicode-unfriendly ^\X?abc
    // Skipping Unicode-unfriendly \X*abc
    // Skipping Unicode-unfriendly \X*abc
    // Skipping Unicode-unfriendly \X*abc
    // Skipping Unicode-unfriendly \X*abc
    // Skipping Unicode-unfriendly ^\X*abc
    // Skipping Unicode-unfriendly [\p{Nd}]
    // Skipping Unicode-unfriendly [\p{Nd}]
    // Skipping Unicode-unfriendly [\P{Nd}]+
    test_eq(run1_match("\\D+", "", "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"), "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
    test_eq(run1_match("\\D+", "", " "), " ");
    test_eq(run1_match("\\D+", "", "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"), "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
    test_eq(run1_match("[\\D]+", "", "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"), "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
    test_eq(run1_match("[\\D\\P{Nd}]+", "", "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"), "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"); // "[\\D\\P{Nd}]+"
    // Skipping Unicode-unfriendly ^[\X]
    // Skipping Unicode-unfriendly ^(\X*)(.)
    // Skipping Unicode-unfriendly ^(\X*)(.)
    // Skipping Unicode-unfriendly ^(\X*?)(.)
    // Skipping Unicode-unfriendly ^(\X*?)(.)
    test_eq(run1_match("^[\\p{Any}]X", "", "AXYZ"), "AX"); // "^[\\p{Any}]X"
    // Skipping Unicode-unfriendly ^[\P{Any}]X
    test_eq(run1_match("^[\\p{Any}]?X", "", "XYZ"), "X");
    test_eq(run1_match("^[\\p{Any}]?X", "", "AXYZ"), "AX");
    test_eq(run1_match("^[\\P{Any}]?X", "", "XYZ"), "X"); // "^[\\P{Any}]?X"
    // Skipping Unicode-unfriendly ^[\P{Any}]?X
    test_eq(run1_match("^[\\p{Any}]+X", "", "AXYZ"), "AX"); // "^[\\p{Any}]+X"
    // Skipping Unicode-unfriendly ^[\P{Any}]+X
    test_eq(run1_match("^[\\p{Any}]*X", "", "XYZ"), "X");
    test_eq(run1_match("^[\\p{Any}]*X", "", "AXYZ"), "AX");
    test_eq(run1_match("^[\\P{Any}]*X", "", "XYZ"), "X"); // "^[\\P{Any}]*X"
    // Skipping Unicode-unfriendly ^[\P{Any}]*X
}
