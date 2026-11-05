//! Effect values as JSON text, with every number carried as written
//! (`K-EFF-002`, `K-EFF-005`).
//!
//! A tool's result is JSON text. [`datum_from_json`] reads it into the tree
//! the machine takes, keeping each number as its [`NumberToken`]: no digit
//! is lost before the machine decodes the number by the `perform`'s stated
//! type or the manifest's bare-number policy. [`datum_to_json`] writes an
//! effect's arguments through the kernel library's common host/text encoder.
//! Host transport preserves canonical `$lash_function` values and escapes guest
//! keys in that reserved prefix. The encoder owns omission and path errors.

use lash_kernel_doc::{Datum, NumberToken};
pub use lash_kernel_lib::JsonModel;

use lash_kernel_value::data_limits::KernelDataLimits;

/// Encode guest data with the shared host/text JSON traversal.
/// The context names the tool, read or output boundary in every path error.
pub fn datum_to_json(
    value: &Datum,
    model: JsonModel,
    context: &str,
) -> Result<String, lash_kernel_doc::NativeError> {
    datum_to_json_with_limits(value, model, context, KernelDataLimits::standard())
}

pub fn datum_to_json_with_limits(
    value: &Datum,
    model: JsonModel,
    context: &str,
    limits: KernelDataLimits,
) -> Result<String, lash_kernel_doc::NativeError> {
    lash_kernel_lib::encode_datum(
        value,
        model,
        lash_kernel_lib::JsonTarget::Host,
        context,
        limits.document_depth(),
    )
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

/// Decode an operation's JSON transport using its declared consumer type.
/// Timestamp strings carry nanoseconds; byte strings carry lowercase hex.
/// These are ordinary type encodings, independent of operation names.
pub fn datum_from_tool_json(
    value: &serde_json::Value,
    expected: &lash_kernel_value::types::Type,
) -> Result<Datum, lash_kernel_doc::DecodeRefusal> {
    fn decode(
        value: &serde_json::Value,
        expected: &lash_kernel_value::types::Type,
        depth: usize,
    ) -> Result<Datum, lash_kernel_doc::DecodeRefusal> {
        use lash_kernel_doc::{Bytes, DecodeRefusal, Integer, Timestamp};
        let mismatch = || DecodeRefusal::Type {
            mismatch: lash_kernel_value::TypeMismatch::new(
                expected.clone(),
                json_value_type(value),
            ),
        };

        use lash_kernel_value::types::Type;
        if depth > KernelDataLimits::standard().document_depth() {
            return Err(DecodeRefusal::DepthLimit {
                limit: KernelDataLimits::standard().document_depth(),
            });
        }
        let array = || value.as_array().ok_or_else(mismatch);
        match expected {
            Type::Timestamp => {
                let text = value.as_str().ok_or_else(mismatch)?;
                let nanoseconds = Integer::parse(text).map_err(|_| mismatch())?;
                if nanoseconds.to_string() != text {
                    return Err(mismatch());
                }
                Ok(Datum::Timestamp(Timestamp { nanoseconds }))
            }
            Type::Bytes => Ok(Datum::Bytes(
                Bytes::parse_hex(value.as_str().ok_or_else(mismatch)?).map_err(|_| mismatch())?,
            )),
            Type::List(item) | Type::Set(item) => {
                let values = array()?
                    .iter()
                    .enumerate()
                    .map(|(index, value)| {
                        decode(value, item, depth + 1)
                            .map_err(|error| error.within(&index.to_string()))
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                Ok(if matches!(expected, Type::Set(_)) {
                    Datum::Set(values)
                } else {
                    Datum::List(values)
                })
            }
            Type::Tuple(items) => {
                let values = array()?;
                if items.len() != values.len() {
                    return Err(mismatch());
                }
                Ok(Datum::Tuple(
                    values
                        .iter()
                        .zip(items)
                        .enumerate()
                        .map(|(index, (value, item))| {
                            decode(value, item, depth + 1)
                                .map_err(|error| error.within(&index.to_string()))
                        })
                        .collect::<Result<_, _>>()?,
                ))
            }
            Type::Record(record) => {
                let object = value.as_object().ok_or_else(mismatch)?;
                let mut fields = Vec::with_capacity(object.len());
                for (name, value) in object {
                    if matches!(record.rest, lash_kernel_value::RecordRest::Drop)
                        && !record.fields.iter().any(|field| field.name == *name)
                    {
                        continue;
                    }
                    let ty = record
                        .fields
                        .iter()
                        .find(|field| field.name == *name)
                        .map(|field| &field.ty)
                        .or(record.rest.kept_type())
                        .ok_or_else(|| DecodeRefusal::Type {
                            mismatch: lash_kernel_value::TypeMismatch::new(
                                Type::Absent,
                                json_value_type(value),
                            )
                            .within(name),
                        })?;
                    fields.push((
                        name.clone(),
                        decode(value, ty, depth + 1).map_err(|error| error.within(name))?,
                    ));
                }
                Ok(Datum::Record(fields))
            }
            Type::Map(map) if map.key == Type::Text => {
                let object = value.as_object().ok_or_else(mismatch)?;
                Ok(Datum::Map(
                    object
                        .iter()
                        .map(|(key, value)| {
                            Ok((
                                Datum::Text(key.clone()),
                                decode(value, &map.value, depth + 1)
                                    .map_err(|error| error.within(key))?,
                            ))
                        })
                        .collect::<Result<_, DecodeRefusal>>()?,
                ))
            }
            Type::Union(variants) => {
                for variant in variants {
                    let datum = match decode(value, variant, depth + 1) {
                        Ok(datum) => datum,
                        Err(DecodeRefusal::Type { .. }) => continue,
                        Err(other) => return Err(other),
                    };
                    {
                        let decoder = lash_kernel_doc::Decoder {
                            max_depth: KernelDataLimits::standard().document_depth(),
                            policy: lash_kernel_doc::NumberPolicy::BySpelling,
                            declared: &|_| false,
                        };
                        match decoder.decode(&datum, variant) {
                            Ok(_) => return Ok(datum),
                            Err(DecodeRefusal::Type { .. }) => {}
                            Err(other) => return Err(other),
                        }
                    }
                }
                Err(mismatch())
            }
            _ => datum_from_json(&value.to_string()).map_err(|error| match error.depth_limit {
                Some(limit) => DecodeRefusal::DepthLimit { limit },
                None => mismatch(),
            }),
        }
    }
    decode(value, expected, 0)
}

/// A bounded witness of the JSON value's kind; no data enters a diagnostic.
fn json_value_type(value: &serde_json::Value) -> lash_kernel_value::Type {
    use {lash_kernel_value::RecordType, lash_kernel_value::Type};
    match value {
        serde_json::Value::Null => Type::Null,
        serde_json::Value::Bool(_) => Type::Bool,
        serde_json::Value::Number(_) => Type::Number,
        serde_json::Value::String(_) => Type::Text,
        serde_json::Value::Array(_) => Type::List(Box::new(Type::Json)),
        serde_json::Value::Object(_) => Type::Record(RecordType {
            fields: vec![],
            rest: lash_kernel_value::RecordRest::Keep(Box::new(Type::Json)),
        }),
    }
}

/// Convert an admitted read answer to the kernel boundary. Provider panics
/// leave through the owner channel, never through a guest error value.
pub fn read_answer(
    expected: &lash_kernel_value::types::Type,
    answer: Result<serde_json::Value, lash_protocol_types::ReadFailure>,
) -> Result<Result<Datum, lash_kernel_doc::ErrorDatum>, lash_protocol_types::ReadPanic> {
    use lash_protocol_types::ReadFailure;
    let error =
        |kind: &str, message: String, data: serde_json::Value| lash_kernel_doc::ErrorDatum {
            kind: kind.into(),
            message,
            data: datum_from_json(&data.to_string()).unwrap_or(Datum::Null),
        };
    Ok(match answer {
        Ok(value) => datum_from_tool_json(&value, expected).map_err(|refusal| {
            refusal.error_datum().unwrap_or_else(|failure| {
                error(
                    "diagnostic_encoding",
                    failure.to_string(),
                    serde_json::Value::Null,
                )
            })
        }),
        Err(ReadFailure::Panicked(panic)) => return Err(panic),
        Err(ReadFailure::Arguments(message)) => {
            Err(error("tool_arguments", message, serde_json::Value::Null))
        }
        Err(ReadFailure::Tool(failure)) => Err(error(
            if failure.code == "tool_call_limit" {
                "tool_call_limit"
            } else {
                "tool_failed"
            },
            failure.message.clone(),
            failure.to_json_value(),
        )),
        Err(ReadFailure::Reader { message, data }) => Err(error("tool_failed", message, data)),
        Err(ReadFailure::Cancelled) => Err(error(
            super::EFFECT_CANCELLED,
            "the read was stopped with its owner".into(),
            serde_json::Value::Null,
        )),
        Err(ReadFailure::TimedOut) => Err(error(
            super::EFFECT_TIMED_OUT,
            "the read exceeded its tool or parent deadline".into(),
            serde_json::Value::Null,
        )),
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]
    #[test]
    fn typed_tool_scalars_do_not_reinterpret_plain_json_text() {
        use lash_kernel_doc::Datum;
        use lash_kernel_value::types::Type;
        let timestamp = serde_json::json!("1700000000123456789");
        let bytes = serde_json::json!("0000000000000001");
        let Datum::Timestamp(actual) =
            super::datum_from_tool_json(&timestamp, &Type::Timestamp).unwrap()
        else {
            panic!("expected timestamp");
        };
        assert_eq!(actual.nanoseconds.to_string(), "1700000000123456789");
        let Datum::Bytes(actual) = super::datum_from_tool_json(&bytes, &Type::Bytes).unwrap()
        else {
            panic!("expected bytes");
        };
        assert_eq!(actual.as_slice(), &[0, 0, 0, 0, 0, 0, 0, 1]);
        assert_eq!(
            super::datum_from_tool_json(&timestamp, &Type::Text).unwrap(),
            Datum::Text("1700000000123456789".into())
        );
        assert!(super::datum_from_tool_json(&serde_json::json!("+1"), &Type::Timestamp).is_err());
    }

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
            datum_to_json(&Datum::List(vec![value]), JsonModel::Kernel, "tool").expect("JSON"),
            r#"[{"column":"n"}]"#
        );
        let error = datum_to_json(
            &Datum::Record(vec![("__lash_built".into(), built)]),
            JsonModel::Kernel,
            "tool",
        )
        .unwrap_err();
        assert!(error.to_string().contains("tool $.__lash_built"));
    }

    /// `K-EFF-002`: an integer leaves as its digits at any size, a whole
    /// float below 2^53 as an integer and any other float as digits that
    /// read back as the same float; what JSON cannot carry is refused by
    /// kind.
    #[test]
    fn arguments_leave_without_loss_or_are_refused_by_kind() {
        let big = Integer::parse("18446744073709551617").expect("an integer");
        let written = datum_to_json(
            &Datum::Record(vec![
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
            ]),
            JsonModel::Kernel,
            "tool",
        )
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
            datum_to_json(
                &Datum::Float(Float::new(f64::NAN)),
                JsonModel::Kernel,
                "tool"
            )
            .unwrap(),
            "null"
        );
        assert_eq!(
            datum_to_json(&Datum::List(vec![Datum::Absent]), JsonModel::Kernel, "tool").unwrap(),
            "[null]"
        );
        assert!(
            datum_to_json(
                &Datum::Map(vec![(Datum::Bool(true), Datum::Null)]),
                JsonModel::Kernel,
                "tool"
            )
            .unwrap_err()
            .to_string()
            .contains("tool $")
        );
    }

    /// FIG-7637: unsupported arguments name both the operation and the path.
    #[test]
    fn unsupported_tool_arguments_name_their_path() {
        for (value, path) in [
            (
                Datum::Record(vec![(
                    "workflow_id".into(),
                    Datum::Bytes(lash_kernel_doc::Bytes::new(vec![])),
                )]),
                "$.workflow_id",
            ),
            (
                Datum::Record(vec![(
                    "a".into(),
                    Datum::List(vec![Datum::Null, Datum::Set(vec![])]),
                )]),
                "$.a[1]",
            ),
        ] {
            let refusal = datum_to_json(&value, JsonModel::Kernel, "linear.issues").unwrap_err();
            assert!(
                refusal
                    .to_string()
                    .contains(&format!("linear.issues {path}")),
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
