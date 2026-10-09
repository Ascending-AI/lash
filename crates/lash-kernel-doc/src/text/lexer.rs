//! Kernel text tokens.

use std::str::FromStr;

use num_bigint::BigInt;

use super::{ParseError, ParseErrorReason};
use crate::name::{FunctionId, parse_hex};
use crate::number::{Float, Integer};

#[derive(Clone, Debug, PartialEq)]
pub(super) enum Token {
    /// A bare word: a keyword or a name.
    Word(String),
    /// A name between backticks: never a keyword.
    Quoted(String),
    Text(String),
    Bytes(Vec<u8>),
    Int(Integer),
    Float(Float),
    /// `@` and 64 hexadecimal digits.
    Identity(FunctionId),
    LParen,
    RParen,
    LBrace,
    RBrace,
    LBracket,
    RBracket,
    Comma,
    Colon,
    Dot,
    DotDot,
    Equals,
    Arrow,
    Question,
    Ampersand,
    End,
}

impl Token {
    pub(super) fn describe(&self) -> String {
        match self {
            Self::Word(word) => format!("`{word}`"),
            Self::Quoted(_) => "a quoted name".to_string(),
            Self::Text(_) => "a text".to_string(),
            Self::Bytes(_) => "bytes".to_string(),
            Self::Int(_) | Self::Float(_) => "a number".to_string(),
            Self::Identity(_) => "an identity".to_string(),
            Self::LParen => "`(`".to_string(),
            Self::RParen => "`)`".to_string(),
            Self::LBrace => "`{`".to_string(),
            Self::RBrace => "`}`".to_string(),
            Self::LBracket => "`[`".to_string(),
            Self::RBracket => "`]`".to_string(),
            Self::Comma => "`,`".to_string(),
            Self::Colon => "`:`".to_string(),
            Self::Dot => "`.`".to_string(),
            Self::DotDot => "`..`".to_string(),
            Self::Equals => "`=`".to_string(),
            Self::Arrow => "`->`".to_string(),
            Self::Question => "`?`".to_string(),
            Self::Ampersand => "`&`".to_string(),
            Self::End => "the end of the text".to_string(),
        }
    }
}

#[derive(Clone, Debug)]
pub(super) struct Spanned {
    pub(super) token: Token,
    pub(super) line: u32,
    pub(super) column: u32,
}

struct Lexer<'a> {
    chars: std::iter::Peekable<std::str::Chars<'a>>,
    line: u32,
    column: u32,
}

/// Splits a kernel text into tokens, ending with [`Token::End`].
pub(super) fn lex(text: &str) -> Result<Vec<Spanned>, ParseError> {
    let mut lexer = Lexer {
        chars: text.chars().peekable(),
        line: 1,
        column: 1,
    };
    let mut tokens = Vec::new();
    loop {
        lexer.skip_blank();
        let (line, column) = (lexer.line, lexer.column);
        let token = lexer.token()?;
        let end = token == Token::End;
        tokens.push(Spanned {
            token,
            line,
            column,
        });
        if end {
            return Ok(tokens);
        }
    }
}

impl Lexer<'_> {
    fn bump(&mut self) -> Option<char> {
        let c = self.chars.next()?;
        if c == '\n' {
            self.line += 1;
            self.column = 1;
        } else {
            self.column += 1;
        }
        Some(c)
    }

    fn peek(&mut self) -> Option<char> {
        self.chars.peek().copied()
    }

    fn malformed(&self, problem: impl Into<String>) -> ParseError {
        ParseError {
            line: self.line,
            column: self.column,
            reason: ParseErrorReason::Malformed {
                problem: problem.into(),
            },
        }
    }

    fn skip_blank(&mut self) {
        while let Some(c) = self.peek() {
            if c == '#' {
                while self.peek().is_some_and(|c| c != '\n') {
                    self.bump();
                }
            } else if c.is_whitespace() {
                self.bump();
            } else {
                break;
            }
        }
    }

    fn token(&mut self) -> Result<Token, ParseError> {
        let Some(c) = self.peek() else {
            return Ok(Token::End);
        };
        let simple = match c {
            '(' => Some(Token::LParen),
            ')' => Some(Token::RParen),
            '{' => Some(Token::LBrace),
            '}' => Some(Token::RBrace),
            '[' => Some(Token::LBracket),
            ']' => Some(Token::RBracket),
            ',' => Some(Token::Comma),
            ':' => Some(Token::Colon),
            '=' => Some(Token::Equals),
            '?' => Some(Token::Question),
            '&' => Some(Token::Ampersand),
            _ => None,
        };
        if let Some(token) = simple {
            self.bump();
            return Ok(token);
        }
        match c {
            '.' => {
                self.bump();
                if self.peek() == Some('.') {
                    self.bump();
                    Ok(Token::DotDot)
                } else {
                    Ok(Token::Dot)
                }
            }
            '"' => {
                self.bump();
                self.quoted('"').map(Token::Text)
            }
            '`' => {
                self.bump();
                self.quoted('`').map(Token::Quoted)
            }
            '@' => {
                self.bump();
                let digits = self.take_while(|c| c.is_ascii_hexdigit());
                FunctionId::parse(&digits)
                    .map(Token::Identity)
                    .map_err(|error| self.malformed(error.to_string()))
            }
            '-' => {
                self.bump();
                match self.peek() {
                    Some('>') => {
                        self.bump();
                        Ok(Token::Arrow)
                    }
                    Some(c) if c.is_ascii_digit() => self.number(true),
                    Some('i') => {
                        let word = self.take_while(is_word_char);
                        if word == "inf" {
                            Ok(Token::Float(Float::new(f64::NEG_INFINITY)))
                        } else {
                            Err(self.malformed("`-` begins a number, `-inf` or `->`"))
                        }
                    }
                    _ => Err(self.malformed("`-` begins a number, `-inf` or `->`")),
                }
            }
            c if c.is_ascii_digit() => self.number(false),
            c if c.is_ascii_alphabetic() || c == '_' => {
                let word = self.take_while(is_word_char);
                if word == "b" && self.peek() == Some('"') {
                    self.bump();
                    let digits = self.quoted('"')?;
                    return parse_hex(&digits).map(Token::Bytes).ok_or_else(|| {
                        self.malformed("bytes are pairs of lower-case hexadecimal digits")
                    });
                }
                Ok(Token::Word(word))
            }
            other => Err(self.malformed(format!("`{other}` begins no token"))),
        }
    }

    fn take_while(&mut self, keep: impl Fn(char) -> bool) -> String {
        let mut out = String::new();
        while let Some(c) = self.peek().filter(|c| keep(*c)) {
            out.push(c);
            self.bump();
        }
        out
    }

    /// Reads a number after an optional `-`: digits, then an optional
    /// fraction, then an optional exponent. A fraction or an exponent makes
    /// it a float.
    fn number(&mut self, negative: bool) -> Result<Token, ParseError> {
        let mut text = String::new();
        if negative {
            text.push('-');
        }
        text.push_str(&self.take_while(|c| c.is_ascii_digit()));
        let mut float = false;
        let mut ahead = self.chars.clone();
        if ahead.next() == Some('.') && ahead.next().is_some_and(|c| c.is_ascii_digit()) {
            float = true;
            self.bump();
            text.push('.');
            text.push_str(&self.take_while(|c| c.is_ascii_digit()));
        }
        let mut ahead = self.chars.clone();
        if ahead.next() == Some('e') {
            let sign = ahead.peek().copied().filter(|c| *c == '-');
            if sign.is_some() {
                ahead.next();
            }
            if ahead.next().is_some_and(|c| c.is_ascii_digit()) {
                float = true;
                self.bump();
                text.push('e');
                if sign.is_some() {
                    self.bump();
                    text.push('-');
                }
                text.push_str(&self.take_while(|c| c.is_ascii_digit()));
            }
        }
        if float {
            f64::from_str(&text)
                .map(|value| Token::Float(Float::new(value)))
                .map_err(|_| self.malformed(format!("`{text}` is not a float")))
        } else {
            BigInt::from_str(&text)
                .map(|value| Token::Int(Integer::new(value)))
                .map_err(|_| self.malformed(format!("`{text}` is not an integer")))
        }
    }

    /// Reads to the closing `quote`, which the opening one has passed.
    fn quoted(&mut self, quote: char) -> Result<String, ParseError> {
        let mut out = String::new();
        loop {
            let Some(c) = self.bump() else {
                return Err(self.malformed("the text ends inside a quoted token"));
            };
            if c == quote {
                return Ok(out);
            }
            if c != '\\' {
                out.push(c);
                continue;
            }
            match self.bump() {
                Some('n') => out.push('\n'),
                Some('r') => out.push('\r'),
                Some('t') => out.push('\t'),
                Some(c @ ('\\' | '"' | '`')) => out.push(c),
                Some('u') => {
                    if self.bump() != Some('{') {
                        return Err(self.malformed("write a code point as `\\u{…}`"));
                    }
                    let digits = self.take_while(|c| c.is_ascii_hexdigit());
                    if self.bump() != Some('}') {
                        return Err(self.malformed("write a code point as `\\u{…}`"));
                    }
                    let scalar = u32::from_str_radix(&digits, 16)
                        .ok()
                        .and_then(char::from_u32)
                        .ok_or_else(|| {
                            self.malformed(format!("`{digits}` is not a Unicode scalar value"))
                        })?;
                    out.push(scalar);
                }
                _ => return Err(self.malformed("unknown escape")),
            }
        }
    }
}

fn is_word_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

/// Writes `text` between `quote`s with the escapes the lexer reads back.
pub(super) fn write_quoted(text: &str, quote: char, out: &mut String) {
    out.push(quote);
    for c in text.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c == quote => {
                out.push('\\');
                out.push(c);
            }
            c if c.is_control() => out.push_str(&format!("\\u{{{:x}}}", u32::from(c))),
            c => out.push(c),
        }
    }
    out.push(quote);
}
