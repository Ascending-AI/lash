//! A run namespace's values as a NamespaceValue node (FIG-5887 §3, kind 35;
//! §7, `turn_namespaces.graph_root_ref`).
//!
//! Each root binds its plugin to a persistent keyed map. Values and index
//! paths are shared graph children, never an opaque namespace-wide stream.

use lash_core_store::plugin_state::NamespaceBody;

use lash_core_store::checkpoint_graph::{
    DecodeLimits, GraphDecoder, GraphRefusal, GraphStaging, NodeId, NodeKind, NodeSource,
    TokenCodec, TokenError, TokenReader, TokenWriter,
};

/// One namespace's values as stored.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NamespaceValue {
    /// The plugin that owns the namespace.
    pub plugin: String,
    /// Its persistent keyed values graph.
    pub body: NamespaceBody,
}

struct NamespaceRoot {
    plugin: String,
    map: NodeId,
}

impl TokenCodec for NamespaceRoot {
    fn encode(&self, out: &mut TokenWriter) {
        out.sum(2, 2);
        out.text(&self.plugin);
        self.map.encode(out);
    }
    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        let (variant, fields) = input.sum()?;
        if variant != 2 {
            return Err(TokenError::UnsupportedVariant { variant });
        }
        TokenReader::expect_fields(fields, 2)?;
        Ok(Self {
            plugin: input.text()?.to_owned(),
            map: NodeId::decode(input)?,
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
        for node in self.body.published().values() {
            staging.adopt(node.clone())?;
        }
        Ok(staging.encode_value(
            NodeKind::NAMESPACE_VALUE,
            &NamespaceRoot {
                plugin: self.plugin.clone(),
                map: self.body.root(),
            },
        )?)
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
        let value: NamespaceRoot =
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
        let body = NamespaceBody::load(source, value.map, limits)?;
        Ok(Self {
            plugin: value.plugin,
            body,
        })
    }
}
