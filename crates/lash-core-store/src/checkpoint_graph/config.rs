//! Canonical configuration objects shared by retained heads and recorded runs.
//!
//! Each object/array names its fields/items by graph reference. Changing one
//! field replaces only its ancestors; unchanged objects keep the same nodes.
//! Resolution happens before staging: reading this graph invokes no producers.

use serde::{Serialize, de::DeserializeOwned};
use serde_json::Value;

use super::GraphDecoder;
use super::GraphStaging;
use super::NodeId;
use super::NodeKind;
use lash_kernel_value::tokens::TokenCodec;
use lash_kernel_value::tokens::TokenError;
use lash_kernel_value::tokens::TokenReader;
use lash_kernel_value::tokens::TokenWriter;

// Preserve the nesting ceiling of the JSON reader these graph objects replace.
const MAX_JSON_DEPTH: usize = 128;

enum ConfigValue {
    Scalar(String),
    Array(Vec<NodeId>),
    Object(Vec<(String, NodeId)>),
}

impl TokenCodec for ConfigValue {
    fn encode(&self, out: &mut TokenWriter) {
        match self {
            Self::Scalar(text) => {
                out.sum(0, 1);
                out.text(text);
            }
            Self::Array(items) => {
                out.sum(1, 1);
                out.seq(items.len() as u64);
                for item in items {
                    item.encode(out);
                }
            }
            Self::Object(fields) => {
                out.sum(2, 1);
                out.seq(fields.len() as u64);
                for (key, value) in fields {
                    out.seq(2);
                    out.text(key);
                    value.encode(out);
                }
            }
        }
    }

    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        let (variant, fields) = input.sum()?;
        TokenReader::expect_fields(fields, 1)?;
        match variant {
            0 => Ok(Self::Scalar(input.text()?.to_owned())),
            1 => {
                let count = input.seq()?;
                let mut items = Vec::new();
                for _ in 0..count {
                    items.push(NodeId::decode(input)?);
                }
                Ok(Self::Array(items))
            }
            2 => {
                let count = input.seq()?;
                let mut fields = Vec::new();
                for _ in 0..count {
                    input.expect_struct(2)?;
                    fields.push((input.text()?.to_owned(), NodeId::decode(input)?));
                }
                Ok(Self::Object(fields))
            }
            variant => Err(TokenError::UnsupportedVariant { variant }),
        }
    }
}

/// Stage immutable resolved data, with one canonical node per value.
///
/// # Errors
///
/// Returns the encoding refusal when the value cannot be serialized or staged.
pub fn stage<T: Serialize + ?Sized>(
    staging: &mut GraphStaging,
    value: &T,
) -> Result<NodeId, String> {
    stage_value(
        staging,
        serde_json::to_value(value).map_err(|error| error.to_string())?,
    )
}

fn stage_value(staging: &mut GraphStaging, value: Value) -> Result<NodeId, String> {
    let body = match value {
        Value::Object(fields) => {
            let fields: std::collections::BTreeMap<_, _> = fields.into_iter().collect();
            ConfigValue::Object(
                fields
                    .into_iter()
                    .map(|(key, value)| stage_value(staging, value).map(|node| (key, node)))
                    .collect::<Result<_, _>>()?,
            )
        }
        Value::Array(items) => ConfigValue::Array(
            items
                .into_iter()
                .map(|value| stage_value(staging, value))
                .collect::<Result<_, _>>()?,
        ),
        scalar => ConfigValue::Scalar(scalar.to_string()),
    };
    staging
        .encode_value(NodeKind::CONFIG_VALUE, &body)
        .map_err(|error| error.to_string())
}

/// Reconstruct exactly the recorded data, without resolving live owners.
///
/// # Errors
///
/// Returns the refusal for a missing, corrupt or incompatible node or value.
pub fn load<T: DeserializeOwned>(
    decoder: &mut GraphDecoder<'_>,
    root: NodeId,
) -> Result<T, String> {
    serde_json::from_value(load_value(decoder, root, 1)?).map_err(|error| error.to_string())
}

fn load_value(decoder: &mut GraphDecoder<'_>, root: NodeId, depth: usize) -> Result<Value, String> {
    let limit = MAX_JSON_DEPTH;
    if depth > limit {
        return Err(format!("config value depth {depth} exceeds limit {limit}"));
    }
    let body: ConfigValue = decoder
        .decode(root, NodeKind::CONFIG_VALUE)
        .map_err(|error| error.to_string())?;
    let mut child = |node| {
        decoder
            .child_kind(root, node, &[NodeKind::CONFIG_VALUE])
            .map_err(|error| error.to_string())?;
        load_value(decoder, node, depth + 1)
    };
    match body {
        ConfigValue::Scalar(text) => {
            let value: Value = serde_json::from_str(&text).map_err(|error| error.to_string())?;
            let canonical_json = value.to_string();
            if value.is_object() || value.is_array() || canonical_json != text {
                return Err("a config scalar is not canonical".to_owned());
            }
            Ok(value)
        }
        ConfigValue::Array(items) => Ok(Value::Array(
            items
                .into_iter()
                .map(&mut child)
                .collect::<Result<_, _>>()?,
        )),
        ConfigValue::Object(fields) => {
            if fields.windows(2).any(|pair| pair[0].0 >= pair[1].0) {
                return Err("config object keys are not strictly ascending".to_owned());
            }
            Ok(Value::Object(
                fields
                    .into_iter()
                    .map(|(key, node)| child(node).map(|value| (key, value)))
                    .collect::<Result<_, _>>()?,
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::checkpoint_graph::{DecodeLimits, EncodeBudget, ExplicitLimit};

    /// The graph representation retains the original JSON reader's nesting
    /// ceiling across references, which each node's token bound cannot see.
    #[test]
    fn configuration_reads_preserve_the_json_nesting_limit() {
        let mut value = Value::Null;
        for _ in 0..MAX_JSON_DEPTH {
            value = Value::Array(vec![value]);
        }
        assert!(
            serde_json::from_str::<Value>(&value.to_string()).is_err(),
            "the original JSON reader refuses this nesting"
        );
        let mut staging = GraphStaging::new(EncodeBudget::new(ExplicitLimit::new(u64::MAX)));
        let root = stage(&mut staging, &value).expect("config graph");
        let mut decoder = GraphDecoder::new(&staging, DecodeLimits::within_total(1 << 20));
        assert_eq!(
            load::<Value>(&mut decoder, root)
                .expect_err("nested config exceeds the original bound"),
            "config value depth 129 exceeds limit 128"
        );
    }

    /// RECORDED-CONFIG-SHARING: a one-field override shares the sticky
    /// namespace and every unchanged field with the historical head.
    #[test]
    fn recorded_config_shares_unchanged_objects_with_revisions() {
        let mut head = crate::store::SessionHeadPayload::default();
        head.config.plugin_config.insert(
            "sticky",
            serde_json::json!({
                "schema": "large sticky plugin configuration ".repeat(8192)
            }),
        );
        let mut staging = GraphStaging::new(EncodeBudget::new(ExplicitLimit::new(u64::MAX)));
        let head_root = stage(&mut staging, &head).expect("head graph");
        let base_root = stage(&mut staging, &head.config).expect("base graph");
        let before = staging.nodes().len();
        let mut effective = head.config.clone();
        effective.max_tool_calls = crate::MaxToolCalls::new(73);
        let run =
            crate::run_spec::ResolvedRun::snapshot(head.config.clone()).with_config(effective);
        let run_root = stage(&mut staging, &run).expect("recorded run graph");
        assert_eq!(
            staging.nodes().len() - before,
            3,
            "only the changed field, its config object and the run root are new"
        );
        let mut decoder = GraphDecoder::new(&staging, DecodeLimits::within_total(8 << 20));
        let ConfigValue::Object(head_fields) = decoder
            .decode(head_root, NodeKind::CONFIG_VALUE)
            .expect("head refs")
        else {
            panic!("head object")
        };
        let ConfigValue::Object(run_fields) = decoder
            .decode(run_root, NodeKind::CONFIG_VALUE)
            .expect("run refs")
        else {
            panic!("run object")
        };
        assert_eq!(
            head_fields
                .iter()
                .find(|(key, _)| key == "config")
                .map(|(_, node)| *node),
            Some(base_root)
        );
        assert_eq!(
            run_fields
                .iter()
                .find(|(key, _)| key == "base")
                .map(|(_, node)| *node),
            Some(base_root)
        );
        let restored: crate::run_spec::ResolvedRun =
            load(&mut decoder, run_root).expect("cold recorded run");
        assert_eq!(
            restored, run,
            "loading reconstructs the original execution config"
        );
    }
}
