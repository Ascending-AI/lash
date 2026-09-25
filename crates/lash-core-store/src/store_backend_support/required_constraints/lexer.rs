//! The lexer the named-`CHECK` inspection runs over stored DDL and rendered
//! expressions: identifiers (bare and quoted), literals, punctuation, casts,
//! JSON text operators and comparisons, in either backend's dialect.

use super::{Comparison, LexMode, Token, TokenKind};

pub(super) fn lex_with_mode(source: &str, mode: LexMode) -> Result<Vec<Token>, String> {
    let bytes = source.as_bytes();
    let mut tokens = Vec::new();
    let mut index = 0;
    while index < bytes.len() {
        let byte = bytes[index];
        if byte.is_ascii_whitespace() {
            index += 1;
            continue;
        }
        if source[index..].starts_with("--") {
            index = source[index..]
                .find('\n')
                .map_or(bytes.len(), |end| index + end + 1);
            continue;
        }
        if source[index..].starts_with("/*") {
            let Some(end) = source[index + 2..].find("*/") else {
                return Err("unterminated block comment".to_string());
            };
            index += end + 4;
            continue;
        }
        let start = index;
        let kind = match byte {
            b'\'' => {
                index += 1;
                let mut value = String::new();
                loop {
                    let Some(next) = bytes.get(index).copied() else {
                        return Err("unterminated string literal".to_string());
                    };
                    if next == b'\'' {
                        if bytes.get(index + 1) == Some(&b'\'') {
                            value.push('\'');
                            index += 2;
                        } else {
                            index += 1;
                            break;
                        }
                    } else {
                        let character = source[index..]
                            .chars()
                            .next()
                            .ok_or_else(|| "invalid UTF-8 boundary".to_string())?;
                        value.push(character);
                        index += character.len_utf8();
                    }
                }
                TokenKind::String(value)
            }
            b'"' | b'`' => {
                let closing = byte;
                index += 1;
                let mut value = String::new();
                loop {
                    let Some(next) = bytes.get(index).copied() else {
                        return Err("unterminated quoted identifier".to_string());
                    };
                    if next == closing {
                        if bytes.get(index + 1) == Some(&closing) && closing != b']' {
                            value.push(closing as char);
                            index += 2;
                        } else {
                            index += 1;
                            break;
                        }
                    } else {
                        let character = source[index..]
                            .chars()
                            .next()
                            .ok_or_else(|| "invalid UTF-8 boundary".to_string())?;
                        value.push(character);
                        index += character.len_utf8();
                    }
                }
                TokenKind::QuotedIdent(if mode == LexMode::Sqlite {
                    value.to_ascii_lowercase()
                } else {
                    value
                })
            }
            b'(' => {
                index += 1;
                TokenKind::LParen
            }
            b')' => {
                index += 1;
                TokenKind::RParen
            }
            b'[' if mode == LexMode::Sqlite => {
                index += 1;
                let mut value = String::new();
                loop {
                    let Some(next) = bytes.get(index).copied() else {
                        return Err("unterminated bracket-quoted identifier".to_string());
                    };
                    if next == b']' {
                        index += 1;
                        break;
                    }
                    let character = source[index..]
                        .chars()
                        .next()
                        .ok_or_else(|| "invalid UTF-8 boundary".to_string())?;
                    value.push(character);
                    index += character.len_utf8();
                }
                TokenKind::QuotedIdent(value.to_ascii_lowercase())
            }
            b'[' => {
                index += 1;
                TokenKind::LBracket
            }
            b']' => {
                index += 1;
                TokenKind::RBracket
            }
            b',' => {
                index += 1;
                TokenKind::Comma
            }
            b':' if bytes.get(index + 1) == Some(&b':') => {
                index += 2;
                TokenKind::Cast
            }
            b'-' if bytes.get(index + 1) == Some(&b'>') && bytes.get(index + 2) == Some(&b'>') => {
                index += 3;
                TokenKind::JsonText
            }
            b'=' => {
                index += 1;
                TokenKind::Comparison(Comparison::Equal)
            }
            b'!' if bytes.get(index + 1) == Some(&b'=') => {
                index += 2;
                TokenKind::Comparison(Comparison::NotEqual)
            }
            b'<' if bytes.get(index + 1) == Some(&b'=') => {
                index += 2;
                TokenKind::Comparison(Comparison::LessEqual)
            }
            b'>' if bytes.get(index + 1) == Some(&b'=') => {
                index += 2;
                TokenKind::Comparison(Comparison::GreaterEqual)
            }
            b'<' if bytes.get(index + 1) == Some(&b'>') => {
                index += 2;
                TokenKind::Comparison(Comparison::NotEqual)
            }
            b'<' => {
                index += 1;
                TokenKind::Comparison(Comparison::Less)
            }
            b'>' => {
                index += 1;
                TokenKind::Comparison(Comparison::Greater)
            }
            b'0'..=b'9' => {
                index += 1;
                while bytes.get(index).is_some_and(u8::is_ascii_digit) {
                    index += 1;
                }
                TokenKind::Number(source[start..index].to_string())
            }
            _ if byte.is_ascii_alphabetic() || byte == b'_' => {
                index += 1;
                while bytes
                    .get(index)
                    .is_some_and(|byte| byte.is_ascii_alphanumeric() || *byte == b'_')
                {
                    index += 1;
                }
                TokenKind::Ident(source[start..index].to_ascii_lowercase())
            }
            _ => {
                let character = source[index..]
                    .chars()
                    .next()
                    .ok_or_else(|| "invalid UTF-8 boundary".to_string())?;
                index += character.len_utf8();
                TokenKind::Other(character)
            }
        };
        tokens.push(Token {
            kind,
            start,
            end: index,
        });
    }
    Ok(tokens)
}
