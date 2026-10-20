//! Effect values as JSON text, with every number carried as written
//! (`K-EFF-002`, `K-EFF-005`).
//!
//! A tool's result is JSON text. [`datum_from_json`] reads it into the tree
//! the machine takes, keeping each number as its [`NumberToken`]: no digit
//! is lost before the machine decodes the number by the `perform`'s stated
//! type or the manifest's bare-number policy. [`datum_to_json`] writes an
//! effect's arguments: an integer as its decimal digits at any size, a float
//! as [`Float::json_number`] spells it (a whole one below 2^53 as an
//! integer). A kind JSON cannot carry is refused with a typed [`NotJson`],
//! never approximated. A function value is written in its transport form,
//! `{"$lash_function": <FunctionValue>}` in canonical JSON, and a record key
//! a guest wrote in that reserved prefix is escaped
//! ([`lash_kernel_doc::escape_function_key`]): this encoder alone mints the
//! tag. A package value, a record holding `__lash_brand`, is written as
//! its own fields, without its brand and `__lash_built`.
//!
//! [`Float::json_number`]: lash_kernel_doc::Float::json_number

use lash_kernel_doc::{Datum, KernelDataLimits, NumberToken};

/// The field a package value holds its brand in
/// (`lash_kernel_dialect::BRAND_FIELD`).
const BRAND_FIELD: &str = "__lash_brand";

/// A package value's provenance fields: its brand, and what built it
/// (`lash_kernel_dialect::BUILT_FIELD`).
const PROVENANCE: [&str; 2] = [BRAND_FIELD, "__lash_built"];

/// A value JSON cannot carry.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum NotJson {
    #[error("JSON has no spelling for a non-finite float")]
    NonFiniteFloat,
    #[error("`{path}` is absent (undefined); JSON has no spelling for absent")]
    Absent { path: String },
    #[error("JSON has no spelling for {kind}")]
    Kind { kind: JsonlessKind },
    #[error("a JSON object's keys are texts, and this map has a key of another kind")]
    MapKey,
    #[error("the value nests deeper than {limit}")]
    TooDeep { limit: usize },
    #[error("a function value does not encode: {problem}")]
    FunctionValue { problem: String },
}

/// The kinds of datum JSON has no spelling for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JsonlessKind {
    Bytes,
    Timestamp,
    Set,
    Error,
    Function,
    Handle,
}

impl std::fmt::Display for JsonlessKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Bytes => "bytes",
            Self::Timestamp => "a timestamp",
            Self::Set => "a set",
            Self::Error => "an error",
            Self::Function => "a function reference",
            Self::Handle => "a handle",
        })
    }
}

/// `value` as JSON text. A tuple and a list are arrays; a record, and a map
/// whose keys are all texts, are objects in their own order.
///
/// # Errors
///
/// [`NotJson`] for a kind JSON cannot carry, anywhere in the tree.
pub fn datum_to_json(value: &Datum) -> Result<String, NotJson> {
    datum_to_json_with_limits(value, KernelDataLimits::standard())
}
pub fn datum_to_json_with_limits(
    value: &Datum,
    limits: KernelDataLimits,
) -> Result<String, NotJson> {
    let mut text = String::new();
    write(value, 0, &mut text, limits.document_depth(), "$")?;
    Ok(text)
}

fn write(
    value: &Datum,
    depth: usize,
    out: &mut String,
    limit: usize,
    path: &str,
) -> Result<(), NotJson> {
    if depth > limit {
        return Err(NotJson::TooDeep { limit });
    }
    let kind = |kind| Err(NotJson::Kind { kind });
    match value {
        Datum::Null => out.push_str("null"),
        Datum::Bool(value) => out.push_str(if *value { "true" } else { "false" }),
        Datum::Int(value) => out.push_str(&value.to_string()),
        Datum::Float(value) => match value.json_number() {
            Some(number) => out.push_str(&number.to_string()),
            None => return Err(NotJson::NonFiniteFloat),
        },
        Datum::Number(token) => out.push_str(token.as_str()),
        Datum::Text(text) => write_text(text, out),
        Datum::Tuple(items) | Datum::List(items) => {
            out.push('[');
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                write(item, depth + 1, out, limit, &format!("{path}[{index}]"))?;
            }
            out.push(']');
        }
        Datum::Record(fields) => {
            // A package value crosses as its own fields: its brand and
            // what built it are provenance, which no runner reads, whatever
            // their encoding.
            let branded = fields.iter().any(|(key, _)| key.as_str() == BRAND_FIELD);
            write_object(
                fields
                    .iter()
                    .filter(|(key, _)| !branded || !PROVENANCE.contains(&key.as_str()))
                    .map(|(key, value)| (key.as_str(), value)),
                depth,
                out,
                limit,
                path,
            )?;
        }
        Datum::Map(entries) => {
            let mut fields = Vec::with_capacity(entries.len());
            for (key, value) in entries {
                let Datum::Text(key) = key else {
                    return Err(NotJson::MapKey);
                };
                fields.push((key.as_str(), value));
            }
            write_object(fields.into_iter(), depth, out, limit, path)?;
        }
        Datum::Absent => {
            return Err(NotJson::Absent {
                path: path.to_owned(),
            });
        }
        Datum::FunctionValue(function) => {
            let tagged =
                function
                    .canonical_tagged_bytes()
                    .map_err(|error| NotJson::FunctionValue {
                        problem: error.to_string(),
                    })?;
            out.push_str(&String::from_utf8_lossy(&tagged));
        }
        Datum::Bytes(_) => return kind(JsonlessKind::Bytes),
        Datum::Timestamp(_) => return kind(JsonlessKind::Timestamp),
        Datum::Set(_) => return kind(JsonlessKind::Set),
        Datum::Error(_) => return kind(JsonlessKind::Error),
        Datum::Function(_) => return kind(JsonlessKind::Function),
        Datum::Handle(_) => return kind(JsonlessKind::Handle),
    }
    Ok(())
}

fn write_object<'a>(
    fields: impl Iterator<Item = (&'a str, &'a Datum)>,
    depth: usize,
    out: &mut String,
    limit: usize,
    path: &str,
) -> Result<(), NotJson> {
    out.push('{');
    for (index, (key, value)) in fields.enumerate() {
        if index > 0 {
            out.push(',');
        }
        write_text(&lash_kernel_doc::escape_function_key(key), out);
        out.push(':');
        let child = if key.chars().enumerate().all(|(i, c)| {
            c == '_' || c == '$' || c.is_ascii_alphabetic() || (i > 0 && c.is_ascii_digit())
        }) && !key.is_empty()
        {
            if path == "$" {
                key.to_owned()
            } else {
                format!("{path}.{key}")
            }
        } else {
            let mut quoted = String::new();
            write_text(key, &mut quoted);
            format!("{path}[{quoted}]")
        };
        write(value, depth + 1, out, limit, &child)?;
    }
    out.push('}');
    Ok(())
}

fn write_text(text: &str, out: &mut String) {
    out.push('"');
    for character in text.chars() {
        match character {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            control if u32::from(control) < 0x20 => {
                out.push_str(&format!("\\u{:04x}", u32::from(control)));
            }
            other => out.push(other),
        }
    }
    out.push('"');
}

/// A text that is not one JSON value.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("not a JSON value at byte {at}: {problem}; depth budget {depth_limit:?}")]
pub struct InvalidJson {
    pub at: usize,
    pub problem: &'static str,
    pub depth_limit: Option<usize>,
}

/// Reads one JSON value (RFC 8259). An array is a list and an object a
/// record with its fields in written order, the last of a repeated key
/// standing; every number is its token, undecoded.
///
/// # Errors
///
/// [`InvalidJson`] for text that is not exactly one JSON value, or one that
/// nests deeper than [`MAX_NESTING_DEPTH`].
pub fn datum_from_json(text: &str) -> Result<Datum, InvalidJson> {
    datum_from_json_with_limits(text, KernelDataLimits::standard())
}
pub fn datum_from_json_with_limits(
    text: &str,
    limits: KernelDataLimits,
) -> Result<Datum, InvalidJson> {
    let mut reader = Reader {
        bytes: text.as_bytes(),
        text,
        at: 0,
        limit: limits.document_depth(),
    };
    reader.space();
    let value = reader.value(0)?;
    reader.space();
    if reader.at != reader.bytes.len() {
        return Err(reader.invalid("text follows the value"));
    }
    Ok(value)
}

struct Reader<'a> {
    bytes: &'a [u8],
    text: &'a str,
    at: usize,
    limit: usize,
}

impl Reader<'_> {
    fn invalid(&self, problem: &'static str) -> InvalidJson {
        InvalidJson {
            at: self.at,
            problem,
            depth_limit: None,
        }
    }

    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.at).copied()
    }

    fn space(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            self.at += 1;
        }
    }

    fn word(&mut self, word: &'static str, value: Datum) -> Result<Datum, InvalidJson> {
        if self.bytes[self.at..].starts_with(word.as_bytes()) {
            self.at += word.len();
            Ok(value)
        } else {
            Err(self.invalid("expected a value"))
        }
    }

    fn value(&mut self, depth: usize) -> Result<Datum, InvalidJson> {
        if depth > self.limit {
            let mut invalid = self.invalid("the value nests too deep");
            invalid.depth_limit = Some(self.limit);
            return Err(invalid);
        }
        match self.peek() {
            Some(b'n') => self.word("null", Datum::Null),
            Some(b't') => self.word("true", Datum::Bool(true)),
            Some(b'f') => self.word("false", Datum::Bool(false)),
            Some(b'"') => self.string().map(Datum::Text),
            Some(b'[') => {
                self.at += 1;
                let mut items = Vec::new();
                self.space();
                if self.peek() == Some(b']') {
                    self.at += 1;
                    return Ok(Datum::List(items));
                }
                loop {
                    self.space();
                    items.push(self.value(depth + 1)?);
                    self.space();
                    match self.peek() {
                        Some(b',') => self.at += 1,
                        Some(b']') => {
                            self.at += 1;
                            return Ok(Datum::List(items));
                        }
                        _ => return Err(self.invalid("expected `,` or `]`")),
                    }
                }
            }
            Some(b'{') => {
                self.at += 1;
                let mut fields: Vec<(String, Datum)> = Vec::new();
                self.space();
                if self.peek() == Some(b'}') {
                    self.at += 1;
                    return Ok(Datum::Record(fields));
                }
                loop {
                    self.space();
                    if self.peek() != Some(b'"') {
                        return Err(self.invalid("expected a key"));
                    }
                    let key = self.string()?;
                    self.space();
                    if self.peek() != Some(b':') {
                        return Err(self.invalid("expected `:`"));
                    }
                    self.at += 1;
                    self.space();
                    let value = self.value(depth + 1)?;
                    match fields.iter_mut().find(|(known, _)| *known == key) {
                        Some(field) => field.1 = value,
                        None => fields.push((key, value)),
                    }
                    self.space();
                    match self.peek() {
                        Some(b',') => self.at += 1,
                        Some(b'}') => {
                            self.at += 1;
                            return Ok(Datum::Record(fields));
                        }
                        _ => return Err(self.invalid("expected `,` or `}`")),
                    }
                }
            }
            Some(b'-' | b'0'..=b'9') => {
                let start = self.at;
                while matches!(
                    self.peek(),
                    Some(b'-' | b'+' | b'.' | b'e' | b'E' | b'0'..=b'9')
                ) {
                    self.at += 1;
                }
                NumberToken::new(&self.text[start..self.at])
                    .map(Datum::Number)
                    .map_err(|_| InvalidJson {
                        at: start,
                        depth_limit: None,
                        problem: "not a number",
                    })
            }
            _ => Err(self.invalid("expected a value")),
        }
    }

    /// The string that opens at the cursor, unescaped by the JSON string
    /// grammar.
    fn string(&mut self) -> Result<String, InvalidJson> {
        let start = self.at;
        self.at += 1;
        loop {
            match self.peek() {
                Some(b'"') => {
                    self.at += 1;
                    break;
                }
                Some(b'\\') => self.at += 2,
                Some(_) => self.at += 1,
                None => return Err(self.invalid("the string does not end")),
            }
        }
        // Both ends sit on an ASCII quote, so the slice is on boundaries; an
        // escape that ran past the end leaves no closing quote and is
        // refused here.
        self.text
            .get(start..self.at)
            .and_then(|quoted| serde_json::from_str::<String>(quoted).ok())
            .ok_or(InvalidJson {
                at: start,
                depth_limit: None,
                problem: "not a string",
            })
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]

    use lash_kernel_doc::{Float, Integer};

    use super::*;

    fn token(text: &str) -> Datum {
        Datum::Number(NumberToken::new(text).expect("a JSON number"))
    }

    /// `K-EFF-005`: a result's numbers arrive as written. No spelling is
    /// reduced to a float on the way in, at any size.
    #[test]
    fn a_result_keeps_every_number_as_it_was_written() {
        let read = datum_from_json(
            r#"{"a":9007199254740993,"b":[18446744073709551617,5.0,1e3,-0,1.50]," c":"xé\n"}"#,
        )
        .expect("JSON");
        assert_eq!(
            read,
            Datum::Record(vec![
                ("a".into(), token("9007199254740993")),
                (
                    "b".into(),
                    Datum::List(vec![
                        token("18446744073709551617"),
                        token("5.0"),
                        token("1e3"),
                        token("-0"),
                        token("1.50"),
                    ])
                ),
                (" c".into(), Datum::Text("x\u{e9}\n".into())),
            ])
        );
    }

    /// A package value crosses as its own fields: its brand and what built
    /// it, provenance no runner reads, are left out whatever their kinds,
    /// and a record without a brand keeps a field of that name.
    #[test]
    fn a_package_value_leaves_as_its_own_fields() {
        let built = Datum::Bytes(lash_kernel_doc::Bytes::new(vec![7u8; 32]));
        let value = Datum::Record(vec![
            (
                "__lash_brand".into(),
                Datum::Tuple(vec![
                    Datum::Text("dataframe".into()),
                    Datum::Text("expr".into()),
                ]),
            ),
            ("__lash_built".into(), built.clone()),
            ("column".into(), Datum::Text("n".into())),
        ]);
        assert_eq!(
            datum_to_json(&Datum::List(vec![value])).expect("JSON"),
            r#"[{"column":"n"}]"#
        );
        assert_eq!(
            datum_to_json(&Datum::Record(vec![("__lash_built".into(), built)])),
            Err(NotJson::Kind {
                kind: JsonlessKind::Bytes
            })
        );
    }

    /// `K-EFF-002`: an integer leaves as its digits at any size, a whole
    /// float below 2^53 as an integer and any other float as digits that
    /// read back as the same float; what JSON cannot carry is refused by
    /// kind.
    #[test]
    fn arguments_leave_without_loss_or_are_refused_by_kind() {
        let big = Integer::parse("18446744073709551617").expect("an integer");
        let written = datum_to_json(&Datum::Record(vec![
            ("int".into(), Datum::Int(big)),
            ("float".into(), Datum::Float(Float::new(5.0))),
            ("tiny".into(), Datum::Float(Float::new(1.5e-7))),
            (
                "map".into(),
                Datum::Map(vec![(
                    Datum::Text("k\"".into()),
                    Datum::Tuple(vec![Datum::Null]),
                )]),
            ),
        ]))
        .expect("JSON");
        assert_eq!(
            written,
            r#"{"int":18446744073709551617,"float":5,"tiny":1.5e-7,"map":{"k\"":[null]}}"#
        );
        assert_eq!(
            datum_from_json(&written).expect("JSON"),
            Datum::Record(vec![
                ("int".into(), token("18446744073709551617")),
                ("float".into(), token("5")),
                ("tiny".into(), token("1.5e-7")),
                (
                    "map".into(),
                    Datum::Record(vec![("k\"".into(), Datum::List(vec![Datum::Null]))])
                ),
            ])
        );
        assert_eq!(
            datum_to_json(&Datum::Float(Float::new(f64::NAN))),
            Err(NotJson::NonFiniteFloat)
        );
        assert_eq!(
            datum_to_json(&Datum::List(vec![Datum::Absent])),
            Err(NotJson::Absent {
                path: "$[0]".into()
            })
        );
        assert_eq!(
            datum_to_json(&Datum::Map(vec![(Datum::Bool(true), Datum::Null)])),
            Err(NotJson::MapKey)
        );
    }

    /// FIG-6431: both cell dialects use this JSON boundary, and an absent
    /// tool argument must name the field the author can correct.
    #[test]
    fn absent_tool_arguments_name_their_path() {
        for (value, path) in [
            (
                Datum::Record(vec![("workflow_id".into(), Datum::Absent)]),
                "workflow_id",
            ),
            (
                Datum::Record(vec![(
                    "a".into(),
                    Datum::Record(vec![(
                        "b".into(),
                        Datum::List(vec![Datum::Null, Datum::Null, Datum::Absent]),
                    )]),
                )]),
                "a.b[2]",
            ),
        ] {
            let refusal = datum_to_json(&value).expect_err("absent is not JSON");
            assert!(
                refusal
                    .to_string()
                    .contains(&format!("`{path}` is absent (undefined)")),
                "{refusal}"
            );
        }
    }

    /// Text that is not exactly one JSON value is refused, never repaired.
    #[test]
    fn text_that_is_not_one_json_value_is_refused() {
        for text in [
            "",
            "01",
            "1.",
            "[1,]",
            "{\"a\":1,}",
            "\"open",
            "1 2",
            "nul",
            "+1",
            "\"\\",
        ] {
            assert!(datum_from_json(text).is_err(), "`{text}` is refused");
        }
    }
}
