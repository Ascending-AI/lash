use lash_kernel_doc::Datum;

/// Lossless data crossing the simple engine boundary. Collections retain order;
/// absent, null, integers, IEEE floats, bytes and timestamps remain distinct.
/// Callables, undecoded JSON numbers and live host handles are refused.
#[derive(Clone, Debug, PartialEq)]
pub struct Value {
    data: Datum,
    bytes: usize,
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ValueError {
    #[error("value exceeds its byte or nesting limit")]
    Limit,
    #[error("callables, number tokens and host handles require advanced embedding")]
    Unsupported,
}

impl Value {
    /// Measures before copying; the cap includes each node's storage and payload.
    pub fn new(data: Datum, max_bytes: usize) -> Result<Self, ValueError> {
        let bytes = measure(&data, max_bytes)?;
        Ok(Self { data, bytes })
    }
    pub fn data(&self) -> &Datum {
        &self.data
    }
    pub fn into_data(self) -> Datum {
        self.data
    }
    pub fn bytes(&self) -> usize {
        self.bytes
    }
    pub(crate) fn copy(data: &Datum, max_bytes: usize) -> Result<Self, ValueError> {
        let bytes = measure(data, max_bytes)?;
        Ok(Self {
            data: data.clone(),
            bytes,
        })
    }
}

fn measure(data: &Datum, max_bytes: usize) -> Result<usize, ValueError> {
    fn visit(data: &Datum, depth: usize, remaining: &mut usize) -> Result<(), ValueError> {
        if depth > 128 {
            return Err(ValueError::Limit);
        }
        let payload = match data {
            Datum::Text(s) => s.len(),
            Datum::Bytes(b) => b.as_slice().len(),
            Datum::Int(i) => {
                usize::try_from(i.bits().div_ceil(8)).map_err(|_| ValueError::Limit)?
            }
            Datum::Timestamp(t) => {
                usize::try_from(t.nanoseconds.bits().div_ceil(8)).map_err(|_| ValueError::Limit)?
            }
            Datum::Function(_) | Datum::FunctionValue(_) | Datum::Handle(_) | Datum::Number(_) => {
                return Err(ValueError::Unsupported);
            }
            _ => 0,
        };
        *remaining = remaining
            .checked_sub(std::mem::size_of::<Datum>())
            .and_then(|n| n.checked_sub(payload))
            .ok_or(ValueError::Limit)?;
        match data {
            Datum::Tuple(v) | Datum::List(v) | Datum::Set(v) => {
                for item in v {
                    visit(item, depth + 1, remaining)?;
                }
            }
            Datum::Map(v) => {
                for (k, v) in v {
                    visit(k, depth + 1, remaining)?;
                    visit(v, depth + 1, remaining)?;
                }
            }
            Datum::Record(v) => {
                for (k, v) in v {
                    *remaining = remaining.checked_sub(k.len()).ok_or(ValueError::Limit)?;
                    visit(v, depth + 1, remaining)?;
                }
            }
            Datum::Error(e) => {
                *remaining = remaining
                    .checked_sub(e.kind.len())
                    .and_then(|n| n.checked_sub(e.message.len()))
                    .ok_or(ValueError::Limit)?;
                visit(&e.data, depth + 1, remaining)?;
            }
            _ => {}
        }
        Ok(())
    }
    let mut remaining = max_bytes;
    visit(data, 0, &mut remaining)?;
    Ok(max_bytes - remaining)
}
