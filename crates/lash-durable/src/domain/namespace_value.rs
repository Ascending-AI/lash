//! A run namespace's values as a NamespaceValue node (FIG-5887 §3, kind 35;
//! §7, `turn_namespaces.graph_root_ref`).
//!
//! A plugin's namespace values are its own bytes, so the node holds them as
//! an explicitly opaque body (variant 1) beside the plugin that owns them.
//! Their bytes are placed like any other: past the inline bound they are
//! shared BytesLeaf nodes, so a namespace that a run rewrites with the same
//! values installs nothing new. Variant 0 is reserved for a first-party
//! typed value.

use std::sync::Arc;

use lash_core_store::checkpoint_graph::{
    DecodeLimits, GraphDecoder, GraphRefusal, GraphStaging, NodeId, NodeKind, NodeSource,
    TokenCodec, TokenError, TokenReader, TokenWriter,
};

/// One namespace's values as stored.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NamespaceValue {
    /// The plugin that owns the namespace.
    pub plugin: String,
    /// Its values, opaque to everything but that plugin.
    pub body: Arc<[u8]>,
}

impl TokenCodec for NamespaceValue {
    fn encode(&self, out: &mut TokenWriter) {
        out.seq(2);
        out.text(&self.plugin);
        out.sum(1, 1);
        out.bytes(&self.body);
    }

    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        input.expect_struct(2)?;
        let plugin = input.text()?.to_owned();
        let (variant, fields) = input.sum()?;
        if variant != 1 {
            return Err(TokenError::UnsupportedVariant { variant });
        }
        TokenReader::expect_fields(fields, 1)?;
        Ok(Self {
            plugin,
            body: input.bytes()?.into(),
        })
    }
}

impl NamespaceValue {
    /// Stages this value and returns its node.
    ///
    /// # Errors
    ///
    /// Any refusal of the staging area.
    pub fn stage(&self, staging: &mut GraphStaging) -> Result<NodeId, GraphRefusal> {
        Ok(staging.encode_value(NodeKind::NAMESPACE_VALUE, self)?)
    }

    /// Reads value `root` of `plugin` from `source`.
    ///
    /// # Errors
    ///
    /// Any refusal of the node read, or [`GraphRefusal::InvalidBody`] for a
    /// value another plugin owns.
    pub fn load(
        source: &dyn NodeSource,
        root: NodeId,
        plugin: &str,
        limits: DecodeLimits,
    ) -> Result<Self, GraphRefusal> {
        let value: Self =
            GraphDecoder::new(source, limits).decode(root, NodeKind::NAMESPACE_VALUE)?;
        if value.plugin != plugin {
            return Err(GraphRefusal::InvalidBody {
                kind: NodeKind::NAMESPACE_VALUE.0,
                reason: format!(
                    "a namespace value of plugin `{}` under `{plugin}`",
                    value.plugin
                ),
            });
        }
        Ok(value)
    }
}
