use super::*;

/// The expansion spliced into `input` the way `string_replace` splices it:
/// the text before the match, the expanded replacement, and the text after.
/// Ranges in `found` are UTF-16 code-unit offsets; every fixture below is
/// ASCII, where they coincide with byte offsets.
fn splice(
    input: &str,
    range: std::ops::Range<usize>,
    captures: &[Option<std::ops::Range<usize>>],
    named: &[(&str, Option<std::ops::Range<usize>>)],
    replacement: &str,
) -> String {
    let heap = Heap::default();
    let units = input.encode_utf16().collect::<Vec<_>>();
    let mut output = units[..range.start].to_vec();
    let mut output_bytes = range.start * 2;
    let found = CapturedMatch {
        range: range.clone(),
        captures: captures.to_vec(),
        named: named
            .iter()
            .map(|(name, range)| (name.to_string(), range.clone()))
            .collect(),
    };
    expand_replacement_checked(
        &heap,
        &mut output,
        &mut output_bytes,
        &units,
        replacement,
        &found,
        lone_surrogate_output_error(true),
    )
    .expect("a small fixture expansion fits every bound");
    output.extend_from_slice(&units[range.end..]);
    String::from_utf16(&output).expect("the spliced result is UTF-16")
}

/// `'foo-x-bar'` with the `x` match at units 4..5 and `n` captures, every
/// one covering that `x` — the shape `/(x)/` and
/// `/((((((((((x))))))))))/` produce at this input.
fn x_match(replacement: &str, capture_count: usize) -> String {
    splice(
        "foo-x-bar",
        4..5,
        &vec![Some(4..5); capture_count],
        &[],
        replacement,
    )
}

/// A string search pattern carries no captures at all (`m = 0`).
fn string_match(replacement: &str) -> String {
    splice("foo-x-bar", 4..5, &[], &[], replacement)
}

/// GetSubstitution's numeric-reference rule: `$0` and `$00` are index 0 and
/// never name a capture, a leading zero is still part of the two-digit
/// form (`$01` is capture 1), and an out-of-range `$nn` falls back to `$n`
/// plus a literal digit before staying literal itself (FIG-3649).
#[test]
fn dollar_digit_substitution_follows_get_substitution() {
    let cases: &[(&str, usize, &str)] = &[
        ("|$0|", 1, "foo-|$0|-bar"),
        ("|$00|", 1, "foo-|$00|-bar"),
        ("|$000|", 1, "foo-|$000|-bar"),
        ("|$01|", 1, "foo-|x|-bar"),
        ("|$010|", 1, "foo-|x0|-bar"),
        ("|$02|", 1, "foo-|$02|-bar"),
        ("|$09|", 1, "foo-|$09|-bar"),
        ("|$10|", 10, "foo-|x|-bar"),
        ("|$10|", 1, "foo-|x0|-bar"),
        ("|$100|", 10, "foo-|x0|-bar"),
        ("|$09|", 10, "foo-|x|-bar"),
        ("|$99|", 10, "foo-|x9|-bar"),
        ("|$91|", 10, "foo-|x1|-bar"),
        ("|$11|", 1, "foo-|x1|-bar"),
        ("|$1|", 1, "foo-|x|-bar"),
        ("|$2|", 1, "foo-|$2|-bar"),
        ("|$39|", 1, "foo-|$39|-bar"),
        ("|$99|", 1, "foo-|$99|-bar"),
    ];
    for (replacement, capture_count, expected) in cases {
        assert_eq!(
            x_match(replacement, *capture_count),
            *expected,
            "{replacement} m={capture_count}"
        );
    }
    // A string pattern has no captures (`m = 0`): every `$n` form is
    // literal, including the leading-zero two-digit forms.
    for (replacement, expected) in [
        ("|$0|", "foo-|$0|-bar"),
        ("|$1|", "foo-|$1|-bar"),
        ("|$01|", "foo-|$01|-bar"),
        ("|$10|", "foo-|$10|-bar"),
    ] {
        assert_eq!(
            string_match(replacement),
            expected,
            "string pattern {replacement}"
        );
    }
}

/// A capture that exists but did not participate substitutes nothing, and
/// the leading-zero form reaches it the same way `$2` does.
#[test]
fn nonparticipating_captures_substitute_nothing() {
    let captures = [Some(4..5), None];
    for (replacement, expected) in [("|$2|", "foo-||-bar"), ("|$02|", "foo-||-bar")] {
        assert_eq!(
            splice("foo-x-bar", 4..5, &captures, &[], replacement),
            expected,
            "{replacement}"
        );
    }
}

/// `$$`, `$&`, `` $` ``, `$'` and `$<name>` are unaffected by the numeric
/// fallback rules; `$<` only opens a name when the match has named groups.
#[test]
fn non_digit_substitution_tokens_are_unchanged() {
    for (replacement, expected) in [
        ("|$$|", "foo-|$|-bar"),
        ("|$&|", "foo-|x|-bar"),
        ("|$`|", "foo-|foo-|-bar"),
        ("|$'|", "foo-|-bar|-bar"),
        ("$", "foo-$-bar"),
        ("$1$", "foo-x$-bar"),
    ] {
        assert_eq!(x_match(replacement, 1), expected, "{replacement}");
    }
    let named = [("n", Some(4..5))];
    for (replacement, expected) in [
        ("|$<n>|", "foo-|x|-bar"),
        ("|$<miss>|", "foo-||-bar"),
        ("|$<x|", "foo-|$<x|-bar"),
    ] {
        assert_eq!(
            splice("foo-x-bar", 4..5, &[Some(4..5)], &named, replacement),
            expected,
            "{replacement}"
        );
    }
    // With no named groups `$<` is not special and stays literal.
    assert_eq!(x_match("|$<n>|", 1), "foo-|$<n>|-bar");
    assert_eq!(string_match("|$<n>|"), "foo-|$<n>|-bar");
}
