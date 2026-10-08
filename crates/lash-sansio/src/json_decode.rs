//! Structural budgets for host JSON envelopes, before any DTO is constructed.

/// Independent allowances for one JSON document. Defaults are 32 MiB of bytes,
/// 1,000,000 nodes, depth 64 and 128 MiB of estimated allocation bytes.
/// Keys and values each count as
/// a node. Depth starts at one for the root, including scalar values and keys.
/// The allocation estimate charges 64 bytes per node plus the encoded content
/// length of every string, which bounds its decoded UTF-8 length. This estimates
/// JSON storage and collection overhead, not arbitrary custom deserializer work.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct JsonDecodeLimits {
    pub max_bytes: usize,
    pub max_nodes: usize,
    pub max_depth: usize,
    pub max_estimated_allocation_bytes: usize,
}

impl JsonDecodeLimits {
    /// Standard preallocation-admission preset: 32 MiB encoded bytes,
    /// 1,000,000 nodes, depth 64 and 128 MiB estimated allocation. Independent
    /// budgets admit structure before allocation; their exact values have no
    /// supporting workload measurements. Serde recursion guards remain independent.
    pub const fn standard() -> Self {
        Self {
            max_bytes: 32 * 1024 * 1024,
            max_nodes: 1_000_000,
            max_depth: 64,
            max_estimated_allocation_bytes: 128 * 1024 * 1024,
        }
    }
}

impl Default for JsonDecodeLimits {
    fn default() -> Self {
        Self::standard()
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct JsonDecodeUsage {
    pub bytes: usize,
    pub nodes: usize,
    pub depth: usize,
    pub estimated_allocation_bytes: usize,
}

#[derive(Debug, thiserror::Error)]
pub enum JsonDecodeError {
    #[error("JSON decode {resource} limit {limit} exceeded by {observed}")]
    LimitExceeded {
        resource: &'static str,
        limit: usize,
        observed: usize,
    },
    #[error("invalid JSON: {0}")]
    Json(#[from] serde_json::Error),
}

fn within(resource: &'static str, observed: usize, limit: usize) -> Result<(), JsonDecodeError> {
    if observed > limit {
        return Err(JsonDecodeError::LimitExceeded {
            resource,
            limit,
            observed,
        });
    }
    Ok(())
}

impl JsonDecodeLimits {
    /// Charge structure without allocating strings, collections or a value tree,
    /// then validate JSON syntax with Serde's non-materializing `IgnoredAny`.
    /// The lexical scan is iterative and holds only counters. Serde's normal
    /// recursion guard still applies independently of the requested depth limit.
    pub fn check(self, bytes: &[u8]) -> Result<JsonDecodeUsage, JsonDecodeError> {
        within("bytes", bytes.len(), self.max_bytes)?;
        std::str::from_utf8(bytes).map_err(|error| {
            JsonDecodeError::Json(<serde_json::Error as serde::de::Error>::custom(error))
        })?;
        let mut usage = JsonDecodeUsage {
            bytes: bytes.len(),
            ..JsonDecodeUsage::default()
        };
        let mut nesting = 0usize;
        let mut cursor = 0usize;
        while cursor < bytes.len() {
            match bytes[cursor] {
                b' ' | b'\n' | b'\r' | b'\t' | b',' | b':' => cursor += 1,
                b'}' | b']' => {
                    nesting = nesting.saturating_sub(1);
                    cursor += 1;
                }
                token => {
                    usage.nodes = usage.nodes.saturating_add(1);
                    within("nodes", usage.nodes, self.max_nodes)?;
                    usage.depth = usage.depth.max(nesting.saturating_add(1));
                    within("depth", usage.depth, self.max_depth)?;
                    usage.estimated_allocation_bytes =
                        usage.estimated_allocation_bytes.saturating_add(64);
                    within(
                        "estimated allocation bytes",
                        usage.estimated_allocation_bytes,
                        self.max_estimated_allocation_bytes,
                    )?;
                    match token {
                        b'{' | b'[' => {
                            nesting = nesting.saturating_add(1);
                            cursor += 1;
                        }
                        b'"' => {
                            cursor += 1;
                            let start = cursor;
                            while cursor < bytes.len() && bytes[cursor] != b'"' {
                                cursor += if bytes[cursor] == b'\\' { 2 } else { 1 };
                            }
                            usage.estimated_allocation_bytes = usage
                                .estimated_allocation_bytes
                                .saturating_add(cursor - start);
                            within(
                                "estimated allocation bytes",
                                usage.estimated_allocation_bytes,
                                self.max_estimated_allocation_bytes,
                            )?;
                            cursor = cursor.saturating_add(1);
                        }
                        _ => {
                            cursor += 1;
                            while cursor < bytes.len()
                                && !matches!(
                                    bytes[cursor],
                                    b' ' | b'\n'
                                        | b'\r'
                                        | b'\t'
                                        | b','
                                        | b':'
                                        | b'['
                                        | b']'
                                        | b'{'
                                        | b'}'
                                        | b'"'
                                )
                            {
                                cursor += 1;
                            }
                        }
                    }
                }
            }
        }
        let _: serde::de::IgnoredAny = serde_json::from_slice(bytes)?;
        Ok(usage)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_boundaries_and_each_independent_refusal() {
        let bytes = br#"{"x":[null,"ab"]}"#;
        let limits = JsonDecodeLimits {
            max_bytes: bytes.len(),
            max_nodes: 5,
            max_depth: 3,
            max_estimated_allocation_bytes: 323,
        };
        assert_eq!(
            limits.check(bytes).unwrap(),
            JsonDecodeUsage {
                bytes: bytes.len(),
                nodes: 5,
                depth: 3,
                estimated_allocation_bytes: 323
            }
        );
        for (resource, tight) in [
            (
                "bytes",
                JsonDecodeLimits {
                    max_bytes: bytes.len() - 1,
                    ..limits
                },
            ),
            (
                "nodes",
                JsonDecodeLimits {
                    max_nodes: 4,
                    ..limits
                },
            ),
            (
                "depth",
                JsonDecodeLimits {
                    max_depth: 2,
                    ..limits
                },
            ),
            (
                "estimated allocation bytes",
                JsonDecodeLimits {
                    max_estimated_allocation_bytes: 322,
                    ..limits
                },
            ),
        ] {
            assert!(
                matches!(tight.check(bytes), Err(JsonDecodeError::LimitExceeded { resource: got, .. }) if got == resource)
            );
        }
    }

    #[test]
    fn escaped_strings_and_duplicate_keys_are_charged_without_decoding() {
        let bytes = br#"{"x":"\u0061\"[]","x":{}}"#;
        let usage = JsonDecodeLimits::default().check(bytes).unwrap();
        assert_eq!(usage.nodes, 5);
        assert_eq!(usage.depth, 2);
        assert_eq!(usage.estimated_allocation_bytes, 64 * 5 + 12);
    }

    #[test]
    fn malformed_json_and_excessive_depth_refuse() {
        for bytes in [
            b"[1,]".as_slice(),
            b"{",
            b"\"\\",
            b"[true false]",
            b"null null",
            b"{\"x\":1,\"y\":}",
            b"\"\xff\"",
        ] {
            assert!(matches!(
                JsonDecodeLimits::default().check(bytes),
                Err(JsonDecodeError::Json(_))
            ));
        }
        let deep = "[".repeat(1000) + &"]".repeat(1000);
        assert!(matches!(
            JsonDecodeLimits::default().check(deep.as_bytes()),
            Err(JsonDecodeError::LimitExceeded {
                resource: "depth",
                ..
            })
        ));
    }
}
