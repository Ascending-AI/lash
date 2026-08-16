use super::super::{
    ErrorKind, JavaScriptUriCodec, RuntimeError, Value, ensure_javascript_string_size,
};
use super::{ExecutionHost, Vm};

const URI_MALFORMED: &str = "URI malformed";

impl<H: ExecutionHost> Vm<'_, H> {
    pub(super) fn execute_javascript_uri_codec(
        &mut self,
        codec: JavaScriptUriCodec,
    ) -> Result<(), RuntimeError> {
        if !self.reference_semantics {
            return Err(RuntimeError::ValidationFailed {
                reason: "TYPESCRIPT_REFERENCE_SEMANTICS_REQUIRED: URI codecs are unavailable in Lashlang"
                    .to_string(),
            });
        }
        let input = self.pop_stack()?;
        let input = self.heap.javascript_to_string(&input)?;
        let result = match codec {
            JavaScriptUriCodec::EncodeComponent => Ok(encode(&input, false)),
            JavaScriptUriCodec::EncodeUri => Ok(encode(&input, true)),
            JavaScriptUriCodec::DecodeComponent => decode(&input, false),
            JavaScriptUriCodec::DecodeUri => decode(&input, true),
        };
        match result {
            Ok(value) => {
                ensure_javascript_string_size(value.len())?;
                self.stack.push(Value::String(value.into()));
                Ok(())
            }
            Err(()) => {
                let value = self.heap.allocate_error(
                    ErrorKind::URIError,
                    URI_MALFORMED.to_string(),
                    None,
                    None,
                )?;
                Err(RuntimeError::UncaughtException { value })
            }
        }
    }
}

fn encode(input: &str, preserve_uri_syntax: bool) -> String {
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

fn decode(input: &str, preserve_uri_syntax: bool) -> Result<String, ()> {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn node_uri_codec_oracles_cover_sets_utf8_and_malformed_sequences() {
        assert_eq!(
            encode("A Z;/?:@&=+$,#-_.!~*'()é😀", false),
            "A%20Z%3B%2F%3F%3A%40%26%3D%2B%24%2C%23-_.!~*'()%C3%A9%F0%9F%98%80"
        );
        assert_eq!(
            encode("https://a.test/a b?x=é&y=#z", true),
            "https://a.test/a%20b?x=%C3%A9&y=#z"
        );
        assert_eq!(
            decode(
                "A%20Z%3B%2F%3F%3A%40%26%3D%2B%24%2C%23%C3%A9%F0%9F%98%80",
                false
            ),
            Ok("A Z;/?:@&=+$,#é😀".to_string())
        );
        assert_eq!(
            decode("https://a.test/a%20b?x=%C3%A9&y=%23z", true),
            Ok("https://a.test/a b?x=é&y=%23z".to_string())
        );
        assert_eq!(
            decode("%3f%23%2F%3A%40%26%3D%2B%24%2C%3B", true),
            Ok("%3f%23%2F%3A%40%26%3D%2B%24%2C%3B".to_string())
        );
        for malformed in ["%", "%0", "%GG", "%C0%AF", "%ED%A0%80", "%E0%A4%A"] {
            assert_eq!(decode(malformed, false), Err(()), "{malformed}");
        }
    }
}
