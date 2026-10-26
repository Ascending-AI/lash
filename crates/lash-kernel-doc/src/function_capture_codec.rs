//! Baseline token encoding for callable capture graphs.
use crate::{CaptureValue, Datum, FunctionValue};
use {
    lash_kernel_value::tokens::TokenCodec, lash_kernel_value::tokens::TokenError,
    lash_kernel_value::tokens::TokenReader, lash_kernel_value::tokens::TokenWriter,
};

impl TokenCodec for CaptureValue {
    fn encode(&self, out: &mut TokenWriter) {
        match self {
            Self::Data(value) => {
                out.sum(0, 1);
                value.encode(out);
            }
            Self::Function(value) => {
                out.sum(1, 1);
                value.encode(out);
            }
            Self::Reference(index) => {
                out.sum(2, 1);
                index.encode(out);
            }
            Self::Tuple(values) => {
                out.sum(3, 1);
                values.encode(out);
            }
            Self::List(values) => {
                out.sum(4, 1);
                values.encode(out);
            }
            Self::Map(values) => {
                out.sum(5, 1);
                values.encode(out);
            }
            Self::Set(values) => {
                out.sum(6, 1);
                values.encode(out);
            }
            Self::Record(values) => {
                out.sum(7, 1);
                values.encode(out);
            }
            Self::Error {
                kind,
                message,
                data,
            } => {
                out.sum(8, 3);
                kind.encode(out);
                message.encode(out);
                data.encode(out);
            }
        }
    }
    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        input.nested(|input| {
            let tag = super::variant(input, |v| match v {
                0..=7 => Some(1),
                8 => Some(3),
                _ => None,
            })?;
            Ok(match tag {
                0 => Self::Data(Datum::decode(input)?),
                1 => Self::Function(Box::<FunctionValue>::decode(input)?),
                2 => Self::Reference(u32::decode(input)?),
                3 => Self::Tuple(Vec::decode(input)?),
                4 => Self::List(Vec::decode(input)?),
                5 => Self::Map(Vec::decode(input)?),
                6 => Self::Set(Vec::decode(input)?),
                7 => Self::Record(Vec::decode(input)?),
                _ => Self::Error {
                    kind: String::decode(input)?,
                    message: String::decode(input)?,
                    data: Box::decode(input)?,
                },
            })
        })
    }
}
