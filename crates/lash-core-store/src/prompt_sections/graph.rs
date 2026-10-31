//! Prompt content uses the immutable graph's codecs and storage identities.

use std::collections::{BTreeMap, BTreeSet};

use super::PromptText;
use crate::checkpoint_graph::{
    DecodeLimits, EncodeBudget, EncodedNode, ExplicitLimit, GraphDecoder, GraphRefusal,
    GraphStaging, NodeId, NodeKind, NodeSource, TypedBody,
};
use crate::store::BlobRef;
use lash_kernel_value::tokens::{TokenCodec, TokenReader, TokenWriter};

fn staging() -> GraphStaging {
    GraphStaging::new(EncodeBudget::new(ExplicitLimit::new(u64::MAX)))
}

fn text(stage: &mut GraphStaging, body: &str) -> Result<NodeId, GraphRefusal> {
    let mut out = TokenWriter::new();
    out.text(body);
    Ok(stage.encode(NodeKind::PROMPT_TEXT, TypedBody::logical(out.as_bytes()))?)
}

fn fragment(
    stage: &mut GraphStaging,
    body: &str,
    children: &[String],
) -> Result<NodeId, GraphRefusal> {
    let mut out = TokenWriter::new();
    out.seq(2);
    out.text(body);
    let children: BTreeSet<_> = children
        .iter()
        .filter_map(|child| NodeId::from_hex(child))
        .collect();
    out.seq(children.len() as u64);
    for child in children {
        child.encode(&mut out);
    }
    Ok(stage.encode(
        NodeKind::PROMPT_FRAGMENT,
        TypedBody::logical(out.as_bytes()),
    )?)
}

pub(super) fn text_reference(body: &str) -> BlobRef {
    text(&mut staging(), body)
        .expect("a prompt text can be placed in bounded graph leaves")
        .blob_ref()
}

pub(super) fn fragment_reference(body: &str, children: &[String]) -> BlobRef {
    fragment(&mut staging(), body, children)
        .expect("prepared request fragments have valid children and bounded bodies")
        .blob_ref()
}

impl PromptText {
    /// Address an audit text in its graph content domain.
    pub fn audit(text: impl Into<std::sync::Arc<str>>) -> Self {
        let text = text.into();
        Self {
            hash: text_reference(&text).0,
            text,
            children: Vec::new(),
        }
    }

    /// Address an immutable fragment with its declared semantic children.
    pub fn fragment(text: impl Into<std::sync::Arc<str>>, children: Vec<NodeId>) -> Self {
        let text = text.into();
        let children = children
            .into_iter()
            .map(|child| child.to_hex())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect::<Vec<_>>();
        Self {
            hash: fragment_reference(&text, &children).0,
            text,
            children,
        }
    }

    /// Encode this content in its audit-text or request-fragment domain.
    /// Only this body's placement is staged; semantic children stay references.
    ///
    /// # Errors
    /// Returns a graph refusal for an invalid identity, body or child reference.
    pub fn graph_nodes(&self) -> Result<BTreeMap<NodeId, EncodedNode>, GraphRefusal> {
        let expected = NodeId::from_hex(&self.hash).ok_or(GraphRefusal::LengthOverflow)?;
        if self
            .children
            .iter()
            .any(|child| NodeId::from_hex(child).is_none())
        {
            return Err(GraphRefusal::LengthOverflow);
        }
        let mut stage = staging();
        if self.children.is_empty() && text(&mut stage, &self.text)? == expected {
            return Ok(stage.into_nodes());
        }
        let mut stage = staging();
        if fragment(&mut stage, &self.text, &self.children)? != expected {
            return Err(GraphRefusal::HashMismatch(expected));
        }
        Ok(stage.into_nodes())
    }

    /// Decode one requested body without hydrating its semantic descendants.
    /// Audit text may read its generic placement leaves through `source`.
    ///
    /// # Errors
    /// Returns the graph decoder's corruption or kind refusal.
    pub fn from_graph(
        source: &dyn NodeSource,
        id: NodeId,
        limits: DecodeLimits,
    ) -> Result<Self, GraphRefusal> {
        let mut decoder = GraphDecoder::new(source, limits);
        let kind = decoder.envelope(id)?.kind;
        if !matches!(kind, NodeKind::PROMPT_TEXT | NodeKind::PROMPT_FRAGMENT) {
            return Err(GraphRefusal::KindMismatch {
                node: id,
                expected: NodeKind::PROMPT_TEXT.0,
                found: kind.0,
            });
        }
        let logical = decoder.logical_body(id, kind)?;
        let mut input = TokenReader::new(&logical);
        let fault = |error| GraphRefusal::from_token(error, Some(id), Some(kind.0));
        if kind == NodeKind::PROMPT_FRAGMENT {
            input.expect_struct(2).map_err(fault)?;
        }
        let text = input.text().map_err(fault)?.into();
        let mut children = Vec::new();
        if kind == NodeKind::PROMPT_FRAGMENT {
            for _ in 0..input.seq().map_err(fault)? {
                children.push(NodeId::decode(&mut input).map_err(fault)?.to_hex());
            }
        }
        input.finish().map_err(fault)?;
        Ok(Self {
            hash: id.to_hex(),
            text,
            children,
        })
    }
}
