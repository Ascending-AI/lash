use super::super::{ErrorKind, RuntimeError, UriCodec, Value, ensure_javascript_string_size};
use super::{ExecutionHost, Vm};

pub(super) const URI_MALFORMED: &str = "URI malformed";

impl<H: ExecutionHost> Vm<'_, H> {
    pub(super) fn execute_javascript_uri_codec(
        &mut self,
        codec: UriCodec,
    ) -> Result<(), RuntimeError> {
        let input = self.pop_stack()?;
        let input = self.heap.javascript_to_string(&input)?;
        // Encoding or decoding reads every input byte once.
        self.charge_intrinsic_work(input.len());
        let result = match codec {
            UriCodec::EncodeComponent => Ok(encode(&input, false)),
            UriCodec::EncodeUri => Ok(encode(&input, true)),
            UriCodec::DecodeComponent => decode(&input, false),
            UriCodec::DecodeUri => decode(&input, true),
        };
        match result {
            Ok(value) => {
                ensure_javascript_string_size(value.len())?;
                self.charge_intrinsic_work(value.len());
                self.stack.push(Value::String(value.into()));
                Ok(())
            }
            Err(()) => {
                let value = self.heap.allocate_error(
                    ErrorKind::URIError,
                    Some(URI_MALFORMED.to_string()),
                    None,
                    None,
                )?;
                Err(RuntimeError::UncaughtException { value })
            }
        }
    }
}

pub(super) fn encode(input: &str, preserve_uri_syntax: bool) -> String {
    let mut output = String::with_capacity(input.len());
    for byte in input.bytes() {
        if is_unescaped(byte) || preserve_uri_syntax && is_uri_syntax(byte) {
            output.push(char::from(byte));
        } else {
            push_percent_encoded(&mut output, byte);
        }
    }
    output
}

pub(super) fn decode(input: &str, preserve_uri_syntax: bool) -> Result<String, ()> {
    let bytes = input.as_bytes();
    let mut output = String::with_capacity(input.len());
    let mut decoded = Vec::new();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] != b'%' {
            flush_decoded(&mut output, &mut decoded)?;
            let character = input[index..].chars().next().ok_or(())?;
            output.push(character);
            index += character.len_utf8();
            continue;
        }
        let byte = decode_hex_pair(bytes.get(index + 1..index + 3).ok_or(())?)?;
        if preserve_uri_syntax && is_uri_syntax(byte) {
            flush_decoded(&mut output, &mut decoded)?;
            output.push_str(&input[index..index + 3]);
        } else {
            decoded.push(byte);
        }
        index += 3;
    }
    flush_decoded(&mut output, &mut decoded)?;
    Ok(output)
}

fn flush_decoded(output: &mut String, decoded: &mut Vec<u8>) -> Result<(), ()> {
    if decoded.is_empty() {
        return Ok(());
    }
    output.push_str(std::str::from_utf8(decoded).map_err(|_| ())?);
    decoded.clear();
    Ok(())
}

fn decode_hex_pair(pair: &[u8]) -> Result<u8, ()> {
    if pair.len() != 2 {
        return Err(());
    }
    let high = hex_value(pair[0]).ok_or(())?;
    let low = hex_value(pair[1]).ok_or(())?;
    Ok((high << 4) | low)
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn push_percent_encoded(output: &mut String, byte: u8) {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    output.push('%');
    output.push(char::from(HEX[(byte >> 4) as usize]));
    output.push(char::from(HEX[(byte & 0x0f) as usize]));
}

fn is_unescaped(byte: u8) -> bool {
    byte.is_ascii_alphanumeric()
        || matches!(
            byte,
            b'-' | b'_' | b'.' | b'!' | b'~' | b'*' | b'\'' | b'(' | b')'
        )
}

fn is_uri_syntax(byte: u8) -> bool {
    matches!(
        byte,
        b';' | b'/' | b'?' | b':' | b'@' | b'&' | b'=' | b'+' | b'$' | b',' | b'#'
    )
}
