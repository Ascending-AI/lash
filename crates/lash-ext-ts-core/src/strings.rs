use lash_kernel_doc::arena::ShortText;
use lash_kernel_doc::{
    Args, NativeCall, NativeError, NativeFunction, NativeResourceContract, PreparedNative, Value,
};

pub(super) struct TextIndex;

fn numeric_index(key: &Value, length: u64) -> Option<Option<usize>> {
    let index = match key {
        Value::Float(value) => value.get(),
        Value::Int(value) => {
            return Some(
                u64::try_from(*value)
                    .ok()
                    .filter(|index| *index < length)
                    .and_then(|index| usize::try_from(index).ok()),
            );
        }
        _ => return None,
    };
    Some(
        (index >= 0.0 && index < length as f64 && index.floor() == index).then_some(index as usize),
    )
}

fn character(text: &str, index: usize) -> Option<Value> {
    let mut units = 0;
    for character in text.chars() {
        if units == index {
            if character.len_utf16() != 1 {
                return None;
            }
            return Some(Value::Text(ShortText::new(
                character.encode_utf8(&mut [0; 4]),
            )?));
        }
        units += character.len_utf16();
        if units > index {
            return None;
        }
    }
    None
}

fn text_index(args: &[Value]) -> Option<Value> {
    let [Value::Text(text), key] = args else {
        return None;
    };
    match numeric_index(key, text.as_str().encode_utf16().count() as u64)? {
        Some(index) => character(text.as_str(), index),
        None => Some(Value::Absent),
    }
}

impl TextIndex {
    fn answer(args: Args<'_>) -> Result<Value, NativeError> {
        let [target, key] = args.values() else {
            return Err(NativeError::Decline);
        };
        let text = args.text(target)?.ok_or(NativeError::Decline)?;
        let index = numeric_index(key, text.summary().utf16_units).ok_or(NativeError::Decline)?;
        let Some(index) = index else {
            return Ok(Value::Absent);
        };
        let mut passed = 0;
        for chunk in text.chunks() {
            let chunk = chunk?;
            let length = chunk.encode_utf16().count();
            if index < passed + length {
                return character(chunk, index - passed).ok_or(NativeError::Decline);
            }
            passed += length;
        }
        Err(NativeError::Decline)
    }
}

impl NativeFunction for TextIndex {
    fn call(&self, call: NativeCall<'_, '_>) -> Result<Value, NativeError> {
        Self::answer(call.args)
    }

    fn resource_contract(
        &self,
        args: Args<'_>,
    ) -> Result<Option<NativeResourceContract>, NativeError> {
        match Self::answer(args) {
            Ok(value) => NativeResourceContract::returned(args, &value).map(Some),
            Err(NativeError::Decline) => Ok(None),
            Err(error) => Err(error),
        }
    }

    fn prepare(&self) -> Option<PreparedNative> {
        Some(PreparedNative {
            pair: None,
            entry: text_index,
            charged_entry: None,
            operation: "ts.text_index(target: Any, key: Any) -> Any",
        })
    }
}

pub(super) struct Uri {
    pub(super) encode: bool,
    pub(super) preserve: bool,
}

struct Output {
    bytes: [u8; ShortText::MAX],
    len: usize,
}

impl Output {
    fn new() -> Self {
        Self {
            bytes: [0; ShortText::MAX],
            len: 0,
        }
    }

    fn push(&mut self, bytes: &[u8]) -> Option<()> {
        let end = self.len.checked_add(bytes.len())?;
        self.bytes.get_mut(self.len..end)?.copy_from_slice(bytes);
        self.len = end;
        Some(())
    }

    fn value(&self) -> Option<Value> {
        Some(Value::Text(ShortText::new(
            std::str::from_utf8(&self.bytes[..self.len]).ok()?,
        )?))
    }
}

fn reserved(byte: u8) -> bool {
    b";/?:@&=+$,#".contains(&byte)
}

fn hex(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

impl Uri {
    fn value(&self, args: Args<'_>) -> Result<Value, NativeError> {
        let [_, arguments] = args.values() else {
            return Err(NativeError::Decline);
        };
        let arguments = args.sequence(arguments)?.ok_or(NativeError::Decline)?;
        let Some(Value::Text(input)) = arguments.get(0)? else {
            return Err(NativeError::Decline);
        };
        self.answer(input.as_str()).ok_or(NativeError::Decline)
    }

    fn answer(&self, input: &str) -> Option<Value> {
        let mut output = Output::new();
        if self.encode {
            for byte in input.bytes() {
                if byte.is_ascii_alphanumeric()
                    || b"-_.!~*'()".contains(&byte)
                    || self.preserve && reserved(byte)
                {
                    output.push(&[byte])?;
                } else {
                    output.push(&[
                        b'%',
                        b"0123456789ABCDEF"[usize::from(byte >> 4)],
                        b"0123456789ABCDEF"[usize::from(byte & 15)],
                    ])?;
                }
            }
        } else {
            let bytes = input.as_bytes();
            let mut at = 0;
            let mut run = Output::new();
            while let Some(byte) = bytes.get(at) {
                if *byte == b'%' {
                    let high = *bytes.get(at + 1)?;
                    let low = *bytes.get(at + 2)?;
                    let byte = hex(high)? * 16 + hex(low)?;
                    if self.preserve && reserved(byte) {
                        std::str::from_utf8(&run.bytes[..run.len]).ok()?;
                        output.push(&run.bytes[..run.len])?;
                        run.len = 0;
                        output.push(&[b'%', high, low])?;
                    } else {
                        run.push(&[byte])?;
                    }
                    at += 3;
                } else {
                    std::str::from_utf8(&run.bytes[..run.len]).ok()?;
                    output.push(&run.bytes[..run.len])?;
                    run.len = 0;
                    let literal = input.get(at..)?.chars().next()?;
                    output.push(literal.encode_utf8(&mut [0; 4]).as_bytes())?;
                    at += literal.len_utf8();
                }
            }
            std::str::from_utf8(&run.bytes[..run.len]).ok()?;
            output.push(&run.bytes[..run.len])?;
        }
        output.value()
    }
}

impl NativeFunction for Uri {
    fn call(&self, call: NativeCall<'_, '_>) -> Result<Value, NativeError> {
        self.value(call.args)
    }

    fn resource_contract(
        &self,
        args: Args<'_>,
    ) -> Result<Option<NativeResourceContract>, NativeError> {
        match self.value(args) {
            Ok(value) => NativeResourceContract::returned(args, &value).map(Some),
            Err(NativeError::Decline) => Ok(None),
            Err(error) => Err(error),
        }
    }
}

pub(super) struct Uppercase;

fn uppercase(args: &[Value]) -> Option<Value> {
    let [Value::Text(text), _] = args else {
        return None;
    };
    let bytes = text.as_bytes();
    if !bytes.is_ascii() {
        return None;
    }
    let mut output = [0; ShortText::MAX];
    for (destination, byte) in output.iter_mut().zip(bytes) {
        *destination = byte.to_ascii_uppercase();
    }
    ShortText::ascii(&output[..bytes.len()]).map(Value::Text)
}

impl NativeFunction for Uppercase {
    fn call(&self, call: NativeCall<'_, '_>) -> Result<Value, NativeError> {
        uppercase(call.args.values()).ok_or(NativeError::Decline)
    }

    fn resource_contract(
        &self,
        args: Args<'_>,
    ) -> Result<Option<NativeResourceContract>, NativeError> {
        uppercase(args.values())
            .map(|value| NativeResourceContract::returned(args, &value))
            .transpose()
    }

    fn prepare(&self) -> Option<PreparedNative> {
        Some(PreparedNative {
            pair: None,
            entry: uppercase,
            charged_entry: None,
            operation: "ts.string.toUpperCase(this: Any, args: List(Any)) -> Text",
        })
    }
}
