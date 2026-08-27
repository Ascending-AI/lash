use serde::Serialize;
use sha2::Digest as _;

pub(crate) fn stable_json_string<T>(value: &T) -> Result<String, serde_json::Error>
where
    T: Serialize + ?Sized,
{
    serde_json::to_string(value)
}

pub(crate) fn blake3_hex(domain: &str, bytes: &[u8]) -> String {
    lash_sansio::core_support::blake3_domain_hash_hex(domain, bytes)
}

/// SHA-256 retained only for contracts whose field or protocol name explicitly
/// promises that algorithm. Lash-owned content and semantic identities use
/// [`blake3_hex`] instead.
pub(crate) fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", sha2::Sha256::digest(bytes))
}

#[cfg(test)]
pub(crate) fn stable_json_sha256_hex<T>(value: &T) -> Result<String, serde_json::Error>
where
    T: Serialize + ?Sized,
{
    serde_json::to_vec(value).map(|bytes| sha256_hex(&bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stable_json_encoding_remains_canonical() {
        assert_eq!(
            stable_json_string(&serde_json::json!({"a": 1, "b": [true, null]}))
                .expect("serialize vector"),
            "{\"a\":1,\"b\":[true,null]}"
        );

        let ordered: serde_json::Value =
            serde_json::from_str(r#"{"a":2,"b":1}"#).expect("parse ordered object");
        let out_of_order: serde_json::Value =
            serde_json::from_str(r#"{"b":1,"a":2}"#).expect("parse out-of-order object");
        let canonical = r#"{"a":2,"b":1}"#;
        for value in [ordered, out_of_order] {
            assert_eq!(
                stable_json_string(&value).expect("serialize object"),
                canonical
            );
        }
    }

    #[test]
    fn blob_v2_blake3_vector_is_pinned() {
        assert_eq!(
            blake3_hex("lash-blob/v2", b"lash-blob-vector"),
            "b2347aac42b4a7b890bad5eda63a4baed377f314a793faa5dfa0e4209d3cba48"
        );
    }
}
