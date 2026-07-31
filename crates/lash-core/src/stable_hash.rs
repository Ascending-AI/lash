use std::io::{self, Write};

use serde::Serialize;
use sha2::Digest;

struct Sha256Writer {
    hasher: sha2::Sha256,
}

impl Write for Sha256Writer {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.hasher.update(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

pub(crate) fn stable_json_string<T>(value: &T) -> Result<String, serde_json::Error>
where
    T: Serialize + ?Sized,
{
    serde_json::to_string(value)
}

pub(crate) fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", sha2::Sha256::digest(bytes))
}

pub(crate) fn stable_json_sha256_hex<T>(value: &T) -> Result<String, serde_json::Error>
where
    T: Serialize + ?Sized,
{
    let mut writer = Sha256Writer {
        hasher: sha2::Sha256::new(),
    };
    serde_json::to_writer(&mut writer, value)?;
    Ok(format!("{:x}", writer.hasher.finalize()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stable_hash_v1_compatibility_corpus() {
        // Versioned durability corpus. These exact bytes and hashes are used
        // in replay/storage identities; changing a vector is a
        // durability-breaking event and requires an explicit format version.
        let json_vectors = [
            (
                serde_json::Value::Null,
                "null",
                "74234e98afe7498fb5daf1f36ac2d78acc339464f950703b8c019892f982b90b",
            ),
            (
                serde_json::json!("lash"),
                "\"lash\"",
                "d6d1bbf708dae2c3693b41240c61b33160eaa7946e4797ba88b4d03f9638233d",
            ),
            (
                serde_json::json!([1, 2, 3]),
                "[1,2,3]",
                "a615eeaee21de5179de080de8c3052c8da901138406ba71c38c032845f7d54f4",
            ),
            (
                serde_json::json!({"a": 1, "b": [true, null]}),
                "{\"a\":1,\"b\":[true,null]}",
                "1cc69c7fa23616ca2ec3ee70d24390a6225c8832db8a4c814c7e0e7f942f8668",
            ),
        ];

        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        for (value, encoded, digest) in json_vectors {
            assert_eq!(
                stable_json_string(&value).expect("serialize vector"),
                encoded
            );
            assert_eq!(stable_json_sha256_hex(&value).expect("hash vector"), digest);
            assert_eq!(sha256_hex(encoded.as_bytes()), digest);
        }

        let ordered: serde_json::Value =
            serde_json::from_str(r#"{"a":2,"b":1}"#).expect("parse ordered object");
        let out_of_order: serde_json::Value =
            serde_json::from_str(r#"{"b":1,"a":2}"#).expect("parse out-of-order object");
        let canonical = r#"{"a":2,"b":1}"#;
        let canonical_digest = "d3626ac30a87e6f7a6428233b3c68299976865fa5508e4267c5415c76af7a772";
        for value in [ordered, out_of_order] {
            assert_eq!(
                stable_json_string(&value).expect("serialize object"),
                canonical
            );
            assert_eq!(
                stable_json_sha256_hex(&value).expect("hash object"),
                canonical_digest
            );
        }
    }
}
