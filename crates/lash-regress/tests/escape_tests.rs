#![allow(clippy::uninlined_format_args)]

use lash_regress::{Regex, escape};

#[test]
fn test_escape_mixed_strings() {
    assert_eq!(escape(""), "");
    assert_eq!(escape("Hello. How are you?"), "Hello\\. How are you\\?");
    assert_eq!(escape("$100 + tax (15%)"), "\\$100 \\+ tax \\(15%\\)");
    assert_eq!(
        escape("a^b$c.d|e?f*g+h(i)j[k]l{m}n\\o"),
        "a\\^b\\$c\\.d\\|e\\?f\\*g\\+h\\(i\\)j\\[k\\]l\\{m\\}n\\\\o"
    );
    assert_eq!(escape("file.txt"), "file\\.txt");
    assert_eq!(escape("user@domain.com"), "user@domain\\.com");
}

#[test]
fn test_escape_with_regex() {
    let test_cases = vec![
        "Hello. How are you?",
        "$100 + tax (15%)",
        "file.txt",
        "user@domain.com",
        "a^b$c.d|e?f*g+h(i)j[k]l{m}n\\o",
        "C:\\Program Files\\MyApp",
        "[brackets] and {braces}",
        ".*+?|^$()[]{}\\",
    ];

    for test_case in test_cases {
        let escaped = escape(test_case);
        let regex = Regex::new(&escaped)
            .unwrap_or_else(|_| panic!("Failed to create regex for: {}", escaped));

        // The escaped pattern should match the original string literally
        assert!(
            regex.find(test_case).is_some(),
            "Escaped pattern '{}' should match original string '{}'",
            escaped,
            test_case
        );

        // The match should be the entire string
        let m = regex.find(test_case).unwrap();
        assert_eq!(
            &test_case[m.range()],
            test_case,
            "Match should be the entire string"
        );
    }
}

#[test]
fn test_escape_unicode() {
    let test_cases = vec![
        "café",
        "привет",
        "你好",
        "🌟 emoji 🎉",
        "混合 text with ünìcödè",
    ];

    for test_case in test_cases {
        let escaped = escape(test_case);
        let regex = Regex::new(&escaped)
            .unwrap_or_else(|_| panic!("Failed to create regex for: {}", escaped));
        assert!(
            regex.find(test_case).is_some(),
            "Escaped pattern should match original unicode string"
        );
    }
}
