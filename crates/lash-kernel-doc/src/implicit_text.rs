//! Source-owned implicit conversion evidence, independent of executable identity.
use lash_vocabulary::tokens::{TokenCodec, TokenError, TokenReader, TokenWriter};

/// What source analysis knows about an operand, independently of runtime values.
/// Functions retain only their known result; objects and arrays distinguish proven
/// representation from annotated fields or elements.
#[derive(
    Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum SourceOperandType {
    Any,
    Undefined,
    Null,
    Boolean,
    Number,
    Text,
    List(Box<Self>),
    Record(std::collections::BTreeMap<String, Self>),
    Object,
    Array,
    Map,
    Set,
    Function { result: Box<Self> },
    Union(Vec<Self>),
    Package { package: String, name: String },
}

/// One implicit conversion computed before lowering erases the operand's type.
#[derive(
    Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
#[serde(deny_unknown_fields)]
pub struct ImplicitTextConversion {
    pub span: Option<crate::Span>,
    pub operand_type: SourceOperandType,
}

impl ImplicitTextConversion {
    pub const CODE: &'static str = "TS_IMPLICIT_TEXT_CONVERSION";

    /// Whether the static operand violates the selected source policy.
    pub fn refused(&self, strict: bool) -> bool {
        fn refused(ty: &SourceOperandType, strict: bool) -> bool {
            match ty {
                SourceOperandType::Any => strict,
                SourceOperandType::Record(_)
                | SourceOperandType::Object
                | SourceOperandType::List(_)
                | SourceOperandType::Array
                | SourceOperandType::Map
                | SourceOperandType::Set
                | SourceOperandType::Function { .. }
                | SourceOperandType::Package { .. } => true,
                SourceOperandType::Union(parts) => parts.iter().any(|part| refused(part, strict)),
                _ => false,
            }
        }
        refused(&self.operand_type, strict)
    }
}

impl TokenCodec for ImplicitTextConversion {
    fn encode(&self, out: &mut TokenWriter) {
        out.seq(2);
        self.span.encode(out);
        self.operand_type.encode(out);
    }
    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        input.expect_struct(2)?;
        Ok(Self {
            span: Option::decode(input)?,
            operand_type: SourceOperandType::decode(input)?,
        })
    }
}

impl TokenCodec for SourceOperandType {
    fn encode(&self, out: &mut TokenWriter) {
        match self {
            Self::Any => {
                out.sum(0, 0);
            }
            Self::Undefined => {
                out.sum(1, 0);
            }
            Self::Null => {
                out.sum(2, 0);
            }
            Self::Boolean => {
                out.sum(3, 0);
            }
            Self::Number => {
                out.sum(4, 0);
            }
            Self::Text => {
                out.sum(5, 0);
            }
            Self::List(inner) => {
                out.sum(6, 1);
                inner.encode(out);
            }
            Self::Record(fields) => {
                out.sum(7, 1);
                fields.encode(out);
            }
            Self::Object => {
                out.sum(8, 0);
            }
            Self::Array => {
                out.sum(9, 0);
            }
            Self::Map => {
                out.sum(10, 0);
            }
            Self::Set => {
                out.sum(11, 0);
            }
            Self::Function { result } => {
                out.sum(12, 1);
                result.encode(out);
            }
            Self::Union(parts) => {
                out.sum(13, 1);
                parts.encode(out);
            }
            Self::Package { package, name } => {
                out.sum(14, 2);
                package.encode(out);
                name.encode(out);
            }
        }
    }
    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        input.nested(|input| {
            let (variant, fields) = input.sum()?;
            let expected = match variant {
                0..=5 | 8..=11 => 0,
                6 | 7 | 12 | 13 => 1,
                14 => 2,
                _ => return Err(TokenError::UnsupportedVariant { variant }),
            };
            TokenReader::expect_fields(fields, expected)?;
            Ok(match variant {
                0 => Self::Any,
                1 => Self::Undefined,
                2 => Self::Null,
                3 => Self::Boolean,
                4 => Self::Number,
                5 => Self::Text,
                6 => Self::List(Box::decode(input)?),
                7 => Self::Record(std::collections::BTreeMap::decode(input)?),
                8 => Self::Object,
                9 => Self::Array,
                10 => Self::Map,
                11 => Self::Set,
                12 => Self::Function {
                    result: Box::decode(input)?,
                },
                13 => Self::Union(Vec::decode(input)?),
                14 => Self::Package {
                    package: String::decode(input)?,
                    name: String::decode(input)?,
                },
                _ => return Err(TokenError::UnsupportedVariant { variant }),
            })
        })
    }
}
