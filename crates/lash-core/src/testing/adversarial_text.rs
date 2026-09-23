//! A deterministic adversarial text domain for generated workloads.
//!
//! Generated payloads used to be `format!`-built ASCII, so multi-byte
//! boundaries never reached truncation, the SQL stores or the journal codecs.
//! [`adversarial_text`] draws instead from a palette that covers every UTF-8
//! encoding width (including 4-byte scalars), combining sequences, a ZWJ
//! sequence, bidi and line-separator controls, `U+0000` and the other C0/C1
//! controls, JSON- and markup-significant characters, and lengths that sit on
//! either side of a byte or line budget with a 4-byte scalar straddling the
//! byte boundary.
//!
//! Every value is a pure function of `(tag, draw, budget)`, so a generator that
//! derives `draw` from its seed stays deterministic per seed. `tag` is kept as
//! an ASCII prefix: it keeps generated values distinct and lets a failure
//! message name which value it was.

/// The budgets a generated text should be able to sit on.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TextBudget {
    /// Byte budget, e.g. the tool-output truncation limit.
    pub bytes: usize,
    /// Line budget, e.g. the tool-output truncation line cap.
    pub lines: usize,
}

/// Pieces a text is assembled from. Every entry is valid UTF-8 that a JSON
/// wire can carry (JSON escapes the controls), so a provider or host can
/// legitimately emit it.
const PIECES: &[&str] = &[
    " plain ascii ",
    "\u{e9}",                     // 2-byte scalar
    "\u{20ac}",                   // 3-byte scalar
    "\u{1d11e}",                  // 4-byte scalar (musical G clef)
    "\u{1f980}",                  // 4-byte scalar (emoji)
    "e\u{301}",                   // base + combining acute
    "\u{1f469}\u{200d}\u{1f4bb}", // ZWJ sequence
    "\u{0}",                      // NUL
    "\u{7}",                      // BEL
    "\u{1b}[0m",                  // ESC sequence
    "\t",
    "\r\n",
    "\n",
    "\u{7f}",     // DEL
    "\u{85}",     // C1 next line
    "\u{2028}",   // line separator
    "\u{202e}",   // right-to-left override
    "\u{feff}",   // BOM / zero-width no-break space
    "\u{fffd}",   // replacement character
    "\u{10ffff}", // last scalar value
    "\"quoted\\back\\slash\"",
    "<tag attr='x'>&amp;</tag>",
    "{\"json\":[1,2]}",
];

/// The 4-byte scalar placed across a byte budget.
const STRADDLE: &str = "\u{1f980}";

/// The shape a draw selects.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Shape {
    Short,
    /// A 4-byte scalar starts `offset` (1..=3) bytes before the byte budget.
    StraddlesByteBudget {
        offset: usize,
    },
    /// Exactly the byte budget.
    ExactByteBudget,
    /// One byte over the byte budget.
    OverByteBudget,
    /// `lines` newline-separated lines, around the line budget.
    AroundLineBudget {
        lines: usize,
    },
}

/// One adversarial text for `(tag, draw)`.
///
/// Ten in sixteen draws are short (a handful of pieces). The rest sit on a
/// budget: a 4-byte scalar straddling the byte budget, exactly the budget,
/// one byte over it, or one line under, at, or over the line budget.
pub fn adversarial_text(tag: &str, draw: u64, budget: TextBudget) -> String {
    let mut state = draw ^ 0x9e37_79b9_7f4a_7c15;
    let shape = match splitmix64(&mut state) % 16 {
        0..=9 => Shape::Short,
        10 | 11 => Shape::StraddlesByteBudget {
            offset: 1 + (splitmix64(&mut state) % 3) as usize,
        },
        12 => Shape::ExactByteBudget,
        13 => Shape::OverByteBudget,
        _ => Shape::AroundLineBudget {
            lines: budget.lines.saturating_sub(1) + (splitmix64(&mut state) % 3) as usize,
        },
    };
    let mut text = tag.to_string();
    match shape {
        Shape::Short => {
            let count = 1 + (splitmix64(&mut state) % 12) as usize;
            for _ in 0..count {
                text.push_str(piece(&mut state));
            }
        }
        Shape::StraddlesByteBudget { offset } => {
            fill_to(&mut text, &mut state, budget.bytes.saturating_sub(offset));
            text.push_str(STRADDLE);
        }
        Shape::ExactByteBudget => fill_to(&mut text, &mut state, budget.bytes),
        Shape::OverByteBudget => fill_to(&mut text, &mut state, budget.bytes.saturating_add(1)),
        Shape::AroundLineBudget { lines } => {
            for line in 1..=lines {
                text.push_str(piece(&mut state).trim_matches(['\n', '\r']));
                text.push_str(&format!(" line {line}\n"));
            }
        }
    }
    text
}

/// A counter value drawn from the scales a provider may report: small, the
/// `u32` boundary, and `i64` scale well below the point where summing a
/// session's worth of them could saturate.
pub fn adversarial_count(draw: u64) -> i64 {
    let mut state = draw ^ 0xd1b5_4a32_d192_ed03;
    let jitter = (splitmix64(&mut state) % 7) as i64;
    match splitmix64(&mut state) % 8 {
        0 => 0,
        1..=3 => 1 + jitter * 97,
        4 => i64::from(u32::MAX) - 3 + jitter,
        5 => i64::from(u32::MAX) + 1 + jitter,
        6 => (1_i64 << 40) + jitter,
        _ => (1_i64 << 53) - 1 - jitter,
    }
}

/// Append pieces, then ASCII padding, until `text` is exactly `target` bytes.
fn fill_to(text: &mut String, state: &mut u64, target: usize) {
    loop {
        let next = piece(state);
        if text.len() + next.len() > target {
            break;
        }
        text.push_str(next);
    }
    while text.len() < target {
        text.push('x');
    }
}

fn piece(state: &mut u64) -> &'static str {
    PIECES[(splitmix64(state) % PIECES.len() as u64) as usize]
}

fn splitmix64(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9e37_79b9_7f4a_7c15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
}

#[cfg(test)]
mod tests {
    use super::*;

    const BUDGET: TextBudget = TextBudget {
        bytes: 256,
        lines: 12,
    };

    #[test]
    fn draws_are_deterministic_and_keep_their_tag() {
        for draw in 0..512 {
            let text = adversarial_text("tag-1 ", draw, BUDGET);
            assert_eq!(text, adversarial_text("tag-1 ", draw, BUDGET));
            assert!(text.starts_with("tag-1 "));
        }
    }

    #[test]
    fn the_domain_reaches_every_class_it_promises() {
        let texts = (0..2048)
            .map(|draw| adversarial_text("t", draw, BUDGET))
            .collect::<Vec<_>>();
        let any = |predicate: &dyn Fn(&str) -> bool| texts.iter().any(|text| predicate(text));
        assert!(any(&|text| text.contains('\u{0}')), "NUL");
        assert!(any(&|text| text.chars().any(|c| c.len_utf8() == 2)));
        assert!(any(&|text| text.chars().any(|c| c.len_utf8() == 3)));
        assert!(any(&|text| text.chars().any(|c| c.len_utf8() == 4)));
        assert!(any(&|text| text.contains('\u{301}')), "combining mark");
        assert!(any(&|text| text.len() == BUDGET.bytes), "exact byte budget");
        assert!(any(&|text| text.len() == BUDGET.bytes + 1), "one over");
        assert!(
            any(&|text| { text.len() > BUDGET.bytes && !text.is_char_boundary(BUDGET.bytes) }),
            "a scalar straddling the byte budget"
        );
        for lines in [BUDGET.lines - 1, BUDGET.lines, BUDGET.lines + 1] {
            assert!(
                any(&|text| text.lines().count() == lines),
                "{lines} lines around the line budget"
            );
        }
    }

    #[test]
    fn counts_cover_small_u32_and_i64_scale() {
        let counts = (0..512).map(adversarial_count).collect::<Vec<_>>();
        assert!(counts.contains(&0));
        assert!(counts.iter().any(|count| (1..1_000).contains(count)));
        assert!(counts.iter().any(|count| *count > i64::from(u32::MAX)));
        assert!(
            counts
                .iter()
                .any(|count| (i64::from(u32::MAX) - 3..=i64::from(u32::MAX)).contains(count))
        );
        assert!(counts.iter().any(|count| *count >= 1_i64 << 52));
        assert!(counts.iter().all(|count| *count < 1_i64 << 53));
    }
}
