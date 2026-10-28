//! Logical occurrence prices shared by source descriptions and stored summaries.
//! These prices describe guest holdings, independently of resident capacities.
use crate::heap::{IntRef, PageValue};

/// The version-one logical held-memory algebra. Code-owned immutable payloads
/// have no run cost; each holding of one pays [`Self::REFERENCE`].
pub struct LogicalHeldV1;

impl LogicalHeldV1 {
    pub const REFERENCE: u64 = 16;
    pub const OBJECT: u64 = 32;
    pub const FRAME: u64 = 64;
    pub const ENTITY_EDGE: u64 = 8;
    pub const BOOKKEEPING: u64 = 0;

    /// Immutable payload additions are per occurrence, including nested tuples.
    #[inline]
    pub fn local(value: &PageValue) -> u64 {
        local_cost(value)
    }

    #[inline]
    pub fn object(members: u64) -> u64 {
        Self::OBJECT.saturating_add(members)
    }

    /// Record keys name fields; their occurrence slots are not guest members.
    #[inline]
    pub fn record(entries: u64, fields: u64) -> u64 {
        Self::object(entries.saturating_sub(fields.saturating_mul(Self::REFERENCE)))
    }

    #[inline]
    pub fn frame(holdings: impl IntoIterator<Item = u64>) -> u64 {
        holdings.into_iter().fold(Self::FRAME, u64::saturating_add)
    }

    #[inline]
    pub fn task(frames: u64, outcome: Option<&PageValue>) -> u64 {
        frames.saturating_add(outcome.map_or(0, Self::local))
    }

    /// The same occurrence algebra on native input views. Cold immutable
    /// members may request a read; chunk geometry never changes the price.
    pub fn native_local(
        args: crate::Args<'_>,
        value: &crate::Value,
    ) -> Result<u64, crate::NativeError> {
        use crate::{NativeError, Value};
        let invalid = || NativeError::Malformed("native logical cost overflow");
        let payload = match value {
            Value::Int(_) | Value::BigInt(_) => {
                args.integer(value)?.ok_or_else(invalid)?.bits().div_ceil(8)
            }
            Value::Timestamp(_) | Value::BigTimestamp(_) => args
                .timestamp(value)?
                .ok_or_else(invalid)?
                .bits()
                .div_ceil(8),
            Value::Text(_) | Value::LongText(_) => args.text(value)?.ok_or_else(invalid)?.len(),
            Value::Bytes(_) => args.bytes(value)?.ok_or_else(invalid)?.len(),
            Value::Tuple(_) => {
                let mut bytes = 0_u64;
                for chunk in args.sequence(value)?.ok_or_else(invalid)?.chunks() {
                    for member in chunk? {
                        bytes = bytes
                            .checked_add(Self::native_local(args, member)?)
                            .ok_or_else(invalid)?;
                    }
                }
                bytes
            }
            Value::Error(_) => {
                let error = args.error(value)?.ok_or_else(invalid)?;
                let data = Self::native_local(args, &error.data)?;
                args.text(&error.kind)?
                    .ok_or_else(invalid)?
                    .len()
                    .checked_add(args.text(&error.message)?.ok_or_else(invalid)?.len())
                    .and_then(|bytes| bytes.checked_add(data))
                    .ok_or_else(invalid)?
            }
            Value::Function(_) => 64,
            Value::Handle(_) => {
                let handle = args.host_handle(value)?.ok_or_else(invalid)?;
                (handle.kind.len() as u64)
                    .checked_add(handle.id.len() as u64)
                    .ok_or_else(invalid)?
            }
            _ => 0,
        };
        Self::REFERENCE.checked_add(payload).ok_or_else(invalid)
    }
}

pub fn local_cost(value: &PageValue) -> u64 {
    16u64.saturating_add(match value {
        PageValue::Int(IntRef::Inline(v)) => v.bits().div_ceil(8),
        PageValue::Int(IntRef::Paged { summary, .. }) => summary.bits.div_ceil(8),
        PageValue::Text(v) => v.summary.utf8_bytes,
        PageValue::Bytes(v) => v.len,
        PageValue::Timestamp(v) => v.nanoseconds.bits().div_ceil(8),
        PageValue::Tuple(v) => v.summary.cost.0,
        PageValue::Error(v) => v.cost.0,
        PageValue::Function(_) => 64,
        PageValue::Handle(v) => (v.kind.len() + v.id.len()) as u64,
        _ => 0,
    })
}
