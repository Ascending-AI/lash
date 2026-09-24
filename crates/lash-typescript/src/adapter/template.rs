//! An untagged template literal's cooked text, per ECMA's `TemplateCharacter`
//! TV (§13.2.8.1).
//!
//! The raw spelling is meaningful only to a tag, which the dialect refuses,
//! so cooking walks the grammar itself — SWC's `cooked` is lenient where ECMA
//! is not (it accepts `\8`, `\9` and `\u{1F_639}`). An escape that cannot cook
//! is the early SyntaxError ECMA writes down, and a cooked lone surrogate is
//! as unrepresentable as the string-literal form.

/// Why one template quasi cannot produce a dialect string.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum TemplateEscapeError {
    /// A `\` starts a `NotEscapeSequence`: a SyntaxError in an untagged
    /// template, where the raw form is legal only under a tag.
    InvalidEscape,
    /// The cooked UTF-16 text holds a lone surrogate, which the value model
    /// cannot carry — the same refusal as a lone-surrogate string literal.
    LoneSurrogate,
}

/// One template quasi's cooked text, per ECMA's `TemplateCharacter` TV.
///
/// The walk is over UTF-16 code units because that is what the grammar cooks:
/// `\uD800` is a valid escape whose TV is a lone surrogate, and `\u{10FFFF}`
/// is a valid escape whose TV is a surrogate pair. Collecting units and
/// decoding once lets `String::from_utf16` pair the halves and single out the
/// lone case, rather than guessing at scalar validity per escape.
pub(super) fn cook_template_quasi(raw: &str) -> Result<String, TemplateEscapeError> {
    fn hex(character: char) -> Option<u32> {
        character.to_digit(16)
    }
    let mut units = Vec::with_capacity(raw.len());
    let mut characters = raw.chars().peekable();
    while let Some(character) = characters.next() {
        if character != '\\' {
            match character {
                // TRV of a LineTerminatorSequence: CR and CRLF both read as
                // LF; LS and PS carry through as themselves.
                '\r' => {
                    if characters.peek() == Some(&'\n') {
                        characters.next();
                    }
                    units.push(u16::from(b'\n'));
                }
                other => units.extend_from_slice(other.encode_utf16(&mut [0; 2])),
            }
            continue;
        }
        let Some(escaped) = characters.next() else {
            return Err(TemplateEscapeError::InvalidEscape);
        };
        match escaped {
            // LineContinuation: contributes nothing.
            '\n' | '\u{2028}' | '\u{2029}' => {}
            '\r' => {
                if characters.peek() == Some(&'\n') {
                    characters.next();
                }
            }
            // SingleEscapeCharacter: the named control characters; `\'`, `"`,
            // `\\` and `` ` `` are their own TVs, covered by the identity arm.
            'b' => units.push(0x0008),
            'f' => units.push(0x000C),
            'n' => units.push(0x000A),
            'r' => units.push(0x000D),
            't' => units.push(0x0009),
            'v' => units.push(0x000B),
            // HexEscapeSequence
            'x' => {
                let hi = characters
                    .next()
                    .and_then(hex)
                    .ok_or(TemplateEscapeError::InvalidEscape)?;
                let lo = characters
                    .next()
                    .and_then(hex)
                    .ok_or(TemplateEscapeError::InvalidEscape)?;
                units.push((hi * 16 + lo) as u16);
            }
            // UnicodeEscapeSequence
            'u' => {
                if characters.peek() == Some(&'{') {
                    characters.next();
                    let mut digits = 0usize;
                    let mut value = 0u32;
                    while let Some(&digit) = characters.peek() {
                        let Some(h) = hex(digit) else { break };
                        characters.next();
                        digits += 1;
                        value = value.saturating_mul(16).saturating_add(h);
                    }
                    if digits == 0 || value > 0x10FFFF || characters.next() != Some('}') {
                        return Err(TemplateEscapeError::InvalidEscape);
                    }
                    // The code point's UTF-16 encoding: one unit in the BMP —
                    // including a surrogate-half unit, which the final decode
                    // refuses — or an astral pair.
                    if let Some(code_point) = char::from_u32(value) {
                        units.extend_from_slice(code_point.encode_utf16(&mut [0; 2]));
                    } else {
                        units.push(value as u16);
                    }
                } else {
                    let mut value = 0u32;
                    for _ in 0..4 {
                        value = value * 16
                            + characters
                                .next()
                                .and_then(hex)
                                .ok_or(TemplateEscapeError::InvalidEscape)?;
                    }
                    units.push(value as u16);
                }
            }
            // `0 [lookahead ∉ DecimalDigit]` is NUL; a digit after it opens a
            // legacy octal escape, which has no untagged form.
            '0' => {
                if characters.peek().is_some_and(|d| d.is_ascii_digit()) {
                    return Err(TemplateEscapeError::InvalidEscape);
                }
                units.push(0);
            }
            // A decimal digit is an EscapeCharacter, so `\8` and `\9` are
            // NotEscapeSequence, never identity escapes.
            '1'..='9' => return Err(TemplateEscapeError::InvalidEscape),
            // SingleEscapeCharacter or NonEscapeCharacter: the character.
            other => units.extend_from_slice(other.encode_utf16(&mut [0; 2])),
        }
    }
    String::from_utf16(&units).map_err(|_| TemplateEscapeError::LoneSurrogate)
}
