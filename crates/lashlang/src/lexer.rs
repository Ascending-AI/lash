use compact_str::CompactString;
use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Span {
    pub start: usize,
    pub end: usize,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Token {
    pub kind: TokenKind,
    pub span: Span,
}

#[derive(Clone, Debug, PartialEq)]
pub enum TokenKind {
    Ident(CompactString),
    String(CompactString),
    Number(f64),
    LBrace,
    RBrace,
    LParen,
    RParen,
    LBracket,
    RBracket,
    Comma,
    Colon,
    At,
    Question,
    Dot,
    Bang,
    Equal,
    DoubleEqual,
    BangEqual,
    AndAnd,
    OrOr,
    Pipe,
    Less,
    LessEqual,
    Greater,
    GreaterEqual,
    Plus,
    Minus,
    Star,
    Slash,
    Percent,
    If,
    Else,
    For,
    In,
    Await,
    Cancel,
    Submit,
    Print,
    Call,
    And,
    Or,
    Not,
    True,
    False,
    Null,
    Eof,
}

macro_rules! define_hard_keywords {
    ($($name:literal => $kind:ident),+ $(,)?) => {
        #[cfg(test)]
        const HARD_KEYWORD_NAMES: &[&str] = &[$($name),+];

        fn hard_keyword_kind(text: &str) -> Option<TokenKind> {
            Some(match text {
                $($name => TokenKind::$kind,)+
                _ => return None,
            })
        }
    };
}

define_hard_keywords! {
    "if" => If,
    "else" => Else,
    "for" => For,
    "in" => In,
    "await" => Await,
    "cancel" => Cancel,
    "submit" => Submit,
    "print" => Print,
    "call" => Call,
    "and" => And,
    "or" => Or,
    "not" => Not,
    "true" => True,
    "false" => False,
    "null" => Null,
}

#[cfg(test)]
pub(crate) fn hard_keyword_names() -> &'static [&'static str] {
    HARD_KEYWORD_NAMES
}

#[derive(Debug, Error, PartialEq)]
pub enum LexError {
    #[error("unexpected `{ch}`")]
    UnexpectedChar { ch: char, offset: usize },
    #[error("unterminated string")]
    UnterminatedString { offset: usize },
    #[error("invalid number `{lexeme}`")]
    InvalidNumber { lexeme: String, offset: usize },
}

impl LexError {
    pub fn offset(&self) -> usize {
        match self {
            Self::UnexpectedChar { offset, .. }
            | Self::UnterminatedString { offset }
            | Self::InvalidNumber { offset, .. } => *offset,
        }
    }
}

pub fn lex(source: &str) -> Result<Vec<Token>, LexError> {
    let mut lexer = Lexer {
        source,
        chars: source.char_indices().peekable(),
    };
    lexer.lex_all()
}

struct Lexer<'a> {
    source: &'a str,
    chars: std::iter::Peekable<std::str::CharIndices<'a>>,
}

impl<'a> Lexer<'a> {
    fn lex_all(&mut self) -> Result<Vec<Token>, LexError> {
        let mut tokens = Vec::with_capacity((self.source.len() / 4).max(8));
        while let Some((offset, ch)) = self.peek() {
            if ch.is_whitespace() || ch == ';' {
                self.bump();
                continue;
            }
            if ch == '#' {
                self.skip_comment();
                continue;
            }
            if ch == '/' && self.peek_second() == Some('/') {
                self.bump();
                self.bump();
                self.skip_comment();
                continue;
            }

            let token = match ch {
                '{' => self.single(TokenKind::LBrace),
                '}' => self.single(TokenKind::RBrace),
                '(' => self.single(TokenKind::LParen),
                ')' => self.single(TokenKind::RParen),
                '[' => self.single(TokenKind::LBracket),
                ']' => self.single(TokenKind::RBracket),
                ',' => self.single(TokenKind::Comma),
                ':' => self.single(TokenKind::Colon),
                '@' => self.single(TokenKind::At),
                '?' => self.single(TokenKind::Question),
                '.' => self.single(TokenKind::Dot),
                '+' => self.single(TokenKind::Plus),
                '-' => self.single(TokenKind::Minus),
                '*' => self.single(TokenKind::Star),
                '/' => self.single(TokenKind::Slash),
                '%' => self.single(TokenKind::Percent),
                '=' => self.double_or_single('=', TokenKind::DoubleEqual, TokenKind::Equal),
                '!' => self.double_or_single('=', TokenKind::BangEqual, TokenKind::Bang),
                '&' => self.required_double('&', TokenKind::AndAnd)?,
                '|' => self.double_or_single('|', TokenKind::OrOr, TokenKind::Pipe),
                '<' => self.double_or_single('=', TokenKind::LessEqual, TokenKind::Less),
                '>' => self.double_or_single('=', TokenKind::GreaterEqual, TokenKind::Greater),
                '"' | '\'' => self.quoted_string(ch)?,
                'r' | 'R' if self.raw_string_delimiter(offset).is_some() => self.raw_string()?,
                c if is_ident_start(c) => self.ident_or_keyword(),
                c if c.is_ascii_digit() => self.number()?,
                _ => return Err(LexError::UnexpectedChar { ch, offset }),
            };
            tokens.push(token);
        }

        let end = self.source.len();
        tokens.push(Token {
            kind: TokenKind::Eof,
            span: Span { start: end, end },
        });
        Ok(tokens)
    }

    fn single(&mut self, kind: TokenKind) -> Token {
        let (start, ch) = self.bump().expect("single token requires input");
        Token {
            kind,
            span: Span {
                start,
                end: start + ch.len_utf8(),
            },
        }
    }

    fn double_or_single(
        &mut self,
        second: char,
        double_kind: TokenKind,
        single_kind: TokenKind,
    ) -> Token {
        let (start, ch) = self.bump().expect("double token requires input");
        let end = if self.consume_if(second) {
            start + ch.len_utf8() + second.len_utf8()
        } else {
            start + ch.len_utf8()
        };
        Token {
            kind: if end > start + ch.len_utf8() {
                double_kind
            } else {
                single_kind
            },
            span: Span { start, end },
        }
    }

    fn quoted_string(&mut self, quote: char) -> Result<Token, LexError> {
        let (start, _) = self.peek().expect("string requires quote");
        let delimiter = string_delimiter(quote, self.starts_with_triple_quote_at(start, quote));
        let content_start = start + delimiter.len();
        self.consume_until_byte(content_start);

        let mut value = String::new();
        while let Some((offset, ch)) = self.bump() {
            if delimiter.len() == 1 {
                if ch == quote {
                    return Ok(Token {
                        kind: TokenKind::String(value.into()),
                        span: Span {
                            start,
                            end: offset + quote.len_utf8(),
                        },
                    });
                }
            } else if self.starts_with_at(offset, &delimiter) {
                self.consume_until_byte(offset + delimiter.len());
                return Ok(Token {
                    kind: TokenKind::String(value.into()),
                    span: Span {
                        start,
                        end: offset + delimiter.len(),
                    },
                });
            }

            if ch == '\\' {
                let Some((_, escaped)) = self.bump() else {
                    return Err(LexError::UnterminatedString { offset: start });
                };
                value.push(translate_escape(escaped, quote));
            } else {
                value.push(ch);
            }
        }
        Err(LexError::UnterminatedString { offset: start })
    }

    fn raw_string_delimiter(&self, offset: usize) -> Option<String> {
        let rest = self.source.get(offset..)?;
        let mut chars = rest.chars();
        match chars.next()? {
            'r' | 'R' => {}
            _ => return None,
        }
        let quote = chars.next()?;
        if quote != '"' && quote != '\'' {
            return None;
        }
        let after_prefix = offset + 1;
        Some(string_delimiter(
            quote,
            self.starts_with_triple_quote_at(after_prefix, quote),
        ))
    }

    fn raw_string(&mut self) -> Result<Token, LexError> {
        let (start, _) = self.peek().expect("raw string requires input");
        let delimiter = self
            .raw_string_delimiter(start)
            .expect("raw string branch requires valid opener");
        let content_start = start + 1 + delimiter.len();

        let Some(relative_end) = self.source[content_start..].find(&delimiter) else {
            return Err(LexError::UnterminatedString { offset: start });
        };
        let content_end = content_start + relative_end;
        let end = content_end + delimiter.len();
        let value = CompactString::from(&self.source[content_start..content_end]);
        self.consume_until_byte(end);
        Ok(Token {
            kind: TokenKind::String(value),
            span: Span { start, end },
        })
    }

    fn required_double(&mut self, expected: char, kind: TokenKind) -> Result<Token, LexError> {
        let (start, ch) = self.bump().expect("double token requires input");
        if !self.consume_if(expected) {
            return Err(LexError::UnexpectedChar { ch, offset: start });
        }
        Ok(Token {
            kind,
            span: Span {
                start,
                end: start + ch.len_utf8() + expected.len_utf8(),
            },
        })
    }

    fn ident_or_keyword(&mut self) -> Token {
        let (start, _) = self.peek().expect("identifier requires input");
        let mut end = start;
        while let Some((offset, ch)) = self.peek() {
            if !is_ident_continue(ch) {
                break;
            }
            end = offset + ch.len_utf8();
            self.bump();
        }
        let text = &self.source[start..end];
        let kind = hard_keyword_kind(text).unwrap_or_else(|| TokenKind::Ident(text.into()));
        Token {
            kind,
            span: Span { start, end },
        }
    }

    fn number(&mut self) -> Result<Token, LexError> {
        let (start, _) = self.peek().expect("number requires input");
        let mut end = start;
        let mut seen_dot = false;
        while let Some((offset, ch)) = self.peek() {
            if ch == '.' && !seen_dot {
                seen_dot = true;
                end = offset + 1;
                self.bump();
                continue;
            }
            if !ch.is_ascii_digit() {
                break;
            }
            end = offset + ch.len_utf8();
            self.bump();
        }
        let lexeme = &self.source[start..end];
        let value = lexeme.parse::<f64>().map_err(|_| LexError::InvalidNumber {
            lexeme: lexeme.to_string(),
            offset: start,
        })?;
        Ok(Token {
            kind: TokenKind::Number(value),
            span: Span { start, end },
        })
    }

    fn skip_comment(&mut self) {
        while let Some((_, ch)) = self.bump() {
            if ch == '\n' {
                break;
            }
        }
    }

    fn bump(&mut self) -> Option<(usize, char)> {
        self.chars.next()
    }

    fn peek(&mut self) -> Option<(usize, char)> {
        self.chars.peek().copied()
    }

    fn peek_second(&self) -> Option<char> {
        let mut chars = self.chars.clone();
        chars.next()?;
        chars.next().map(|(_, ch)| ch)
    }

    fn starts_with_at(&self, offset: usize, needle: &str) -> bool {
        self.source[offset..].starts_with(needle)
    }

    fn starts_with_triple_quote_at(&self, offset: usize, quote: char) -> bool {
        let mut delimiter = String::with_capacity(3);
        delimiter.push(quote);
        delimiter.push(quote);
        delimiter.push(quote);
        self.starts_with_at(offset, &delimiter)
    }

    fn consume_until_byte(&mut self, end: usize) {
        while let Some((offset, _)) = self.peek() {
            if offset >= end {
                break;
            }
            self.bump();
        }
    }

    fn consume_if(&mut self, expected: char) -> bool {
        match self.peek() {
            Some((_, ch)) if ch == expected => {
                self.bump();
                true
            }
            _ => false,
        }
    }
}

fn is_ident_start(ch: char) -> bool {
    ch == '_' || ch.is_ascii_alphabetic()
}

fn is_ident_continue(ch: char) -> bool {
    is_ident_start(ch) || ch.is_ascii_digit()
}

fn string_delimiter(quote: char, triple: bool) -> String {
    let count = if triple { 3 } else { 1 };
    std::iter::repeat_n(quote, count).collect()
}

fn translate_escape(escaped: char, quote: char) -> char {
    match escaped {
        '\\' => '\\',
        'n' => '\n',
        'r' => '\r',
        't' => '\t',
        other if other == quote => quote,
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lexes_all_token_classes_and_comments() {
        let tokens = lex(r#"
            # comment
            // comment
            if else for in await cancel submit print call and or not true false null start
            name _x a1 "hi\n\t\"\\\r\q" 12 3.5 { } ( ) [ ] , : @ ? . ! = == != && || | < <= > >= + - * / %
            "#)
        .expect("lexing should succeed");

        let kinds: Vec<_> = tokens.into_iter().map(|token| token.kind).collect();
        assert_eq!(
            kinds,
            vec![
                TokenKind::If,
                TokenKind::Else,
                TokenKind::For,
                TokenKind::In,
                TokenKind::Await,
                TokenKind::Cancel,
                TokenKind::Submit,
                TokenKind::Print,
                TokenKind::Call,
                TokenKind::And,
                TokenKind::Or,
                TokenKind::Not,
                TokenKind::True,
                TokenKind::False,
                TokenKind::Null,
                TokenKind::Ident("start".into()),
                TokenKind::Ident("name".into()),
                TokenKind::Ident("_x".into()),
                TokenKind::Ident("a1".into()),
                TokenKind::String("hi\n\t\"\\\rq".into()),
                TokenKind::Number(12.0),
                TokenKind::Number(3.5),
                TokenKind::LBrace,
                TokenKind::RBrace,
                TokenKind::LParen,
                TokenKind::RParen,
                TokenKind::LBracket,
                TokenKind::RBracket,
                TokenKind::Comma,
                TokenKind::Colon,
                TokenKind::At,
                TokenKind::Question,
                TokenKind::Dot,
                TokenKind::Bang,
                TokenKind::Equal,
                TokenKind::DoubleEqual,
                TokenKind::BangEqual,
                TokenKind::AndAnd,
                TokenKind::OrOr,
                TokenKind::Pipe,
                TokenKind::Less,
                TokenKind::LessEqual,
                TokenKind::Greater,
                TokenKind::GreaterEqual,
                TokenKind::Plus,
                TokenKind::Minus,
                TokenKind::Star,
                TokenKind::Slash,
                TokenKind::Percent,
                TokenKind::Eof,
            ]
        );
    }

    #[test]
    fn rejects_unexpected_characters() {
        let err = lex("`").expect_err("lexing should fail");
        assert_eq!(err, LexError::UnexpectedChar { ch: '`', offset: 0 });
    }

    #[test]
    fn lexes_python_shaped_string_literals() {
        let tokens = lex(r####"
            double = "hi\n\t\"\\\r\q"
            single = 'it\'s ok\n'
            triple_double = """line1\n"quoted"
line2"""
            triple_single = '''line1\n'quoted'
line2'''
            raw_double = r"*** Begin Patch
@@
\n { untouched }
*** End Patch"
            raw_single = r'path\to\file'
            "####)
        .expect("lexing should succeed");

        let strings: Vec<_> = tokens
            .into_iter()
            .filter_map(|token| match token.kind {
                TokenKind::String(value) => Some(value),
                _ => None,
            })
            .collect();
        assert_eq!(
            strings,
            vec![
                CompactString::from("hi\n\t\"\\\rq"),
                CompactString::from("it's ok\n"),
                CompactString::from("line1\n\"quoted\"\nline2"),
                CompactString::from("line1\n'quoted'\nline2"),
                CompactString::from("*** Begin Patch\n@@\n\\n { untouched }\n*** End Patch"),
                CompactString::from("path\\to\\file"),
            ]
        );
    }

    #[test]
    fn lexes_shell_and_formatter_shaped_string_literals() {
        let tokens = lex(r####"
            date = "date '+%Y-%m-%d %H:%M:%S %Z (%z)'"
            printf = 'printf "%s\\n" "$value"'
            json = "{\"cmd\":\"echo 'ok'\"}"
            shell = "${HOME:-/tmp} && echo %done"
            comment_text = "// not a comment # also not a comment"
            label_text = "@label(title: \"plain\")"
            "####)
        .expect("lexing should succeed");

        let strings: Vec<_> = tokens
            .into_iter()
            .filter_map(|token| match token.kind {
                TokenKind::String(value) => Some(value),
                _ => None,
            })
            .collect();
        assert_eq!(
            strings,
            vec![
                CompactString::from("date '+%Y-%m-%d %H:%M:%S %Z (%z)'"),
                CompactString::from("printf \"%s\\n\" \"$value\""),
                CompactString::from("{\"cmd\":\"echo 'ok'\"}"),
                CompactString::from("${HOME:-/tmp} && echo %done"),
                CompactString::from("// not a comment # also not a comment"),
                CompactString::from("@label(title: \"plain\")"),
            ]
        );
    }

    #[test]
    fn lexes_raw_triple_strings() {
        let tokens = lex(r#####"
            script = r'''python3 - <<'PY'
print("""double quotes are preserved""")
\n { braces stay raw }
PY'''
            markdown = R"""This body can mention " and ' without escaping.
It ends only at three double quotes."""
            "#####)
        .expect("lexing should succeed");

        let strings: Vec<_> = tokens
            .into_iter()
            .filter_map(|token| match token.kind {
                TokenKind::String(value) => Some(value),
                _ => None,
            })
            .collect();
        assert_eq!(
            strings,
            vec![
                CompactString::from(
                    "python3 - <<'PY'\nprint(\"\"\"double quotes are preserved\"\"\")\n\\n { braces stay raw }\nPY",
                ),
                CompactString::from(
                    "This body can mention \" and ' without escaping.\nIt ends only at three double quotes.",
                ),
            ]
        );
    }

    #[test]
    fn lexes_label_annotation_text_inside_strings_as_strings() {
        let tokens = lex(r####"
            regular = "@label(title: \"plain\")"
            multiline = """@label(title: "plain")
finish null"""
            raw = r"""@label(title: "plain")
@label(title: "still plain") finish null"""
            "####)
        .expect("lexing should succeed");

        assert!(
            tokens
                .iter()
                .all(|token| !matches!(token.kind, TokenKind::At)),
            "`@` inside strings must not lex as annotation syntax"
        );
        let strings: Vec<_> = tokens
            .into_iter()
            .filter_map(|token| match token.kind {
                TokenKind::String(value) => Some(value),
                _ => None,
            })
            .collect();
        assert_eq!(
            strings,
            vec![
                CompactString::from("@label(title: \"plain\")"),
                CompactString::from("@label(title: \"plain\")\nfinish null"),
                CompactString::from(
                    "@label(title: \"plain\")\n@label(title: \"still plain\") finish null"
                ),
            ]
        );
    }

    #[test]
    fn lexes_double_slash_comments_without_breaking_division() {
        let tokens = lex(r#"
            value = 6 / 2
            // trailing comment
            submit value
            "#)
        .expect("lexing should succeed");

        let kinds: Vec<_> = tokens.into_iter().map(|token| token.kind).collect();
        assert_eq!(
            kinds,
            vec![
                TokenKind::Ident("value".into()),
                TokenKind::Equal,
                TokenKind::Number(6.0),
                TokenKind::Slash,
                TokenKind::Number(2.0),
                TokenKind::Submit,
                TokenKind::Ident("value".into()),
                TokenKind::Eof,
            ]
        );
    }

    #[test]
    fn rejects_unterminated_strings() {
        let err = lex("\"abc").expect_err("lexing should fail");
        assert_eq!(err, LexError::UnterminatedString { offset: 0 });

        let err = lex("\"abc\\").expect_err("lexing should fail");
        assert_eq!(err, LexError::UnterminatedString { offset: 0 });

        let err = lex("\"\"\"abc").expect_err("lexing should fail");
        assert_eq!(err, LexError::UnterminatedString { offset: 0 });

        let err = lex("'abc").expect_err("lexing should fail");
        assert_eq!(err, LexError::UnterminatedString { offset: 0 });

        let err = lex("'''abc").expect_err("lexing should fail");
        assert_eq!(err, LexError::UnterminatedString { offset: 0 });

        let err = lex("r\"abc").expect_err("lexing should fail");
        assert_eq!(err, LexError::UnterminatedString { offset: 0 });

        let err = lex("r'''abc").expect_err("lexing should fail");
        assert_eq!(err, LexError::UnterminatedString { offset: 0 });
    }

    #[test]
    fn rust_style_raw_strings_are_not_recognized() {
        let tokens = lex("submit r#\"abc\"#").expect("lexing treats hash as comment");
        let kinds: Vec<_> = tokens.into_iter().map(|token| token.kind).collect();
        assert_eq!(
            kinds,
            vec![
                TokenKind::Submit,
                TokenKind::Ident("r".into()),
                TokenKind::Eof,
            ]
        );
    }

    #[test]
    fn internal_number_error_path_is_covered() {
        let mut lexer = Lexer {
            source: ".",
            chars: ".".char_indices().peekable(),
        };
        let err = lexer.number().expect_err("number parsing should fail");
        assert_eq!(
            err,
            LexError::InvalidNumber {
                lexeme: ".".to_string(),
                offset: 0
            }
        );
    }

    #[test]
    fn identifier_helpers_cover_true_and_false_cases() {
        assert!(is_ident_start('_'));
        assert!(is_ident_start('a'));
        assert!(!is_ident_start('1'));

        assert!(is_ident_continue('9'));
        assert!(is_ident_continue('_'));
        assert!(!is_ident_continue('-'));
    }
}
