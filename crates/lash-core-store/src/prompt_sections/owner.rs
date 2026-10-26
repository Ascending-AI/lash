//! A single owner carries immutable prepared prompt paths.
use super::*;
use lash_protocol_types::persistent_sequence::PersistentSequence;

/// Each append links the previous exact prefix to one leaf. Publishing never
/// combines old subtrees, so its direct frontier is independent of prefix
/// length, repeated bodies and the number of leaves in a frame's seed.
#[derive(Clone, Debug, Default)]
struct PreparedPrefix {
    paths: Option<std::sync::Arc<PreparedPaths>>,
    root: Option<std::sync::Arc<PromptText>>,
    pending: Vec<std::sync::Arc<PromptText>>,
    texts: RequestTexts,
    /// The previous prepared directory, held only during a synchronous edit.
    /// Equal fragments rebuilt under new segment identities are still external
    /// references, not new publication bodies.
    prior: Option<crate::persistent_map::PersistentMap<TextKey, TextUse>>,
}

/// An indexed directory, rather than recursively owned chronological links:
/// truncation visits only the discarded suffix and dropping a long prefix
/// never recurses through its prompt links.
#[derive(Clone, Debug, Default)]
struct PreparedPaths {
    leaves: PersistentSequence<PreparedLeaf>,
}

#[derive(Clone, Debug)]
struct PreparedLeaf {
    body: std::sync::Arc<PromptText>,
    prefix_root: std::sync::Arc<PromptText>,
}

impl PreparedPrefix {
    fn len(&self) -> usize {
        self.paths.as_ref().map_or(0, |paths| paths.leaves.len())
    }
    fn paths_mut(&mut self) -> &mut PreparedPaths {
        std::sync::Arc::make_mut(self.paths.get_or_insert_with(Default::default))
    }
    fn unpublish(&mut self) {
        if let Some(root) = self.root.take()
            && self.len() == 0
        {
            self.remove_owned(root.hash.as_str());
        }
    }
    fn publish(&mut self) -> PromptTextRef {
        if self.root.is_none() {
            let root = self
                .paths
                .as_ref()
                .and_then(|paths| paths.leaves.get(paths.leaves.len().checked_sub(1)?))
                .map(|leaf| leaf.prefix_root.clone());
            self.root = Some(if let Some(root) = root {
                root
            } else {
                let (_, root) = encode_fragment(&RequestFragment::Index {
                    children: Vec::new(),
                });
                self.keep_body_owned(root.clone());
                root
            });
        }
        let root = self.root.as_ref().expect("published prefix");
        PromptTextRef {
            blob: BlobRef(root.hash.clone()),
            bytes: u32::try_from(root.text.len()).unwrap_or(u32::MAX),
        }
    }
    fn snapshot(&mut self, root: &PromptTextRef) -> RequestTexts {
        self.prior = None;
        let delta = std::mem::take(&mut self.pending)
            .into_iter()
            .filter(|text| self.texts.get(text.hash.as_str()).is_some())
            .collect::<Vec<_>>()
            .into();
        RequestTexts {
            roots: vec![root.blob.as_str().to_owned()],
            texts: self.texts.texts.clone(),
            len: self.texts.len,
            delta: Some(delta),
        }
    }
    fn keep_body_owned(&mut self, encoded: std::sync::Arc<PromptText>) {
        let count = self
            .texts
            .texts
            .get(encoded.hash.as_str())
            .map_or(0, |entry| entry.count);
        if count == 0 {
            if self
                .prior
                .as_ref()
                .is_none_or(|prior| prior.get(encoded.hash.as_str()).is_none())
            {
                self.pending.push(encoded.clone());
            }
            self.texts.len += 1;
        }
        self.texts.texts.insert(
            TextKey(encoded.clone()),
            TextUse {
                text: encoded,
                count: count + 1,
            },
        );
    }
    fn remove_owned(&mut self, hash: &str) {
        if let Some(entry) = self.texts.texts.get(hash).cloned() {
            if entry.count == 1 {
                self.texts.texts.remove(hash);
                self.texts.len -= 1;
            } else {
                self.texts.texts.insert(
                    TextKey(entry.text.clone()),
                    TextUse {
                        count: entry.count - 1,
                        ..entry
                    },
                );
            }
        }
    }
    fn truncate_owned(&mut self, len: usize) {
        if len == self.len() {
            return;
        }
        self.unpublish();
        if let Some(paths) = self.paths.take() {
            for leaf in paths.leaves.iter_from(len) {
                self.remove_owned(leaf.body.hash.as_str());
                if !std::sync::Arc::ptr_eq(&leaf.body, &leaf.prefix_root) {
                    self.remove_owned(leaf.prefix_root.hash.as_str());
                }
            }
            self.paths = Some(std::sync::Arc::new(PreparedPaths {
                leaves: paths.leaves.prefix(len),
            }));
        }
    }
    fn append_leaves<'a>(&mut self, leaves: impl Iterator<Item = &'a Leaf>) {
        for (_, body) in leaves {
            self.unpublish();
            let older = self
                .paths
                .as_ref()
                .and_then(|paths| paths.leaves.get(paths.leaves.len().checked_sub(1)?))
                .map(|leaf| leaf.prefix_root.clone());
            self.keep_body_owned(body.clone());
            let prefix_root = if let Some(older) = older {
                let root = encode_branch_owned(&older, body);
                self.keep_body_owned(root.clone());
                root
            } else {
                body.clone()
            };
            let paths = self.paths_mut();
            paths.leaves = paths.leaves.append(&[PreparedLeaf {
                body: body.clone(),
                prefix_root,
            }]);
        }
    }
}

/// One acknowledged prompt draft; readers share immutable paths and obligations.
#[derive(Clone, Debug, Default)]
pub struct OwnerPromptState {
    lowering: lash_protocol_types::llm::types::SegmentCache,
    source: std::sync::Arc<lash_protocol_types::llm::types::RequestMessages>,
    items: lash_protocol_types::llm::types::RequestItems,
    opening: Option<lash_protocol_types::llm::types::RequestSegment>,
    prepared: PreparedPrefix,
    boundaries: lash_protocol_types::persistent_sequence::PersistentSequence<usize>,
    opening_leaves: usize,
}
impl OwnerPromptState {
    /// Reconcile a fresh rendering against this frame's acknowledged messages.
    /// Equal values keep their original identities, so provider lowering and
    /// decoding retain one prefix rather than another copy on every send.
    /// The candidate belongs to this private draft until admission adopts it.
    pub fn retain_request(
        &mut self,
        request: &lash_protocol_types::llm::types::RetainedLlmRequest,
    ) -> lash_protocol_types::llm::types::RetainedLlmRequest {
        let shared = self.source.shared_prefix(request.messages());
        let shared = shared
            + self
                .source
                .iter_from(shared)
                .zip(request.messages().iter_from(shared))
                .take_while(|(before, now)| before == now)
                .count();
        self.source = std::sync::Arc::new(if shared == 0 {
            request.messages().clone()
        } else {
            let tail: Vec<_> = request.messages().iter_from(shared).cloned().collect();
            self.source.prefix(shared).append(&tail)
        });
        request.with_messages(self.source.as_ref().clone())
    }
    /// Observe the exact prepared prefix directory without keeping it alive.
    #[cfg(any(test, feature = "testing"))]
    pub fn prepared_source_for_testing(&self) -> Option<std::sync::Weak<dyn Send + Sync>> {
        let source: std::sync::Arc<dyn Send + Sync> = self.prepared.paths.as_ref()?.clone();
        Some(std::sync::Arc::downgrade(&source))
    }

    /// Lowered request nodes have the same claim and frame lifetime as the prompt.
    pub fn lowering_cache(&self) -> &lash_protocol_types::llm::types::SegmentCache {
        &self.lowering
    }
    /// A preparation edits only its private lowering directory; its immutable
    /// source, rendered items and fragment paths remain shared with readers.
    pub fn draft(&self) -> Self {
        let lowering = self.lowering.draft();
        Self {
            lowering,
            ..self.clone()
        }
    }
    #[cfg(test)]
    pub(super) fn retained_leaves(&self) -> usize {
        self.prepared.len()
    }

    pub fn clear(&mut self) {
        *self = Self::default();
    }
    pub fn chunk(
        &mut self,
        template: &lash_protocol_types::llm::types::RecordedRequestTemplate,
    ) -> (ChunkedRequestTemplate, RequestTexts) {
        self.chunk_draft(template)
    }
    fn chunk_draft(
        &mut self,
        template: &lash_protocol_types::llm::types::RecordedRequestTemplate,
    ) -> (ChunkedRequestTemplate, RequestTexts) {
        let Some((opening, items, closing)) = template.retained_items() else {
            self.clear();
            return ChunkedRequestTemplate::chunk_flat(template);
        };
        self.prepared.prior = Some(self.prepared.texts.texts.clone());
        self.prepared.pending = Vec::with_capacity(usize::BITS as usize);
        let opening_same = self.opening.as_ref().is_some_and(|prior| prior == opening);
        let shared = if opening_same {
            self.items.shared_prefix(items)
        } else {
            0
        };
        if !opening_same {
            self.prepared = PreparedPrefix {
                prior: self.prepared.prior.take(),
                ..PreparedPrefix::default()
            };
            self.boundaries = Default::default();
            let mut chunker = TextChunker::new(true);
            let mut leaves = Vec::new();
            for part in opening.parts() {
                match part {
                    lash_protocol_types::llm::types::SegmentPart::Literal(text) => {
                        chunker.literal_owned(text, &mut leaves)
                    }
                    lash_protocol_types::llm::types::SegmentPart::Slot(slot) => {
                        leaves.push(encode_fragment(&RequestFragment::Attachment { slot }))
                    }
                }
            }
            self.prepared.append_leaves(leaves.iter());
            self.opening_leaves = self.prepared.len();
        } else if shared < self.items.len() {
            let len = shared
                .checked_sub(1)
                .and_then(|index| self.boundaries.get(index))
                .copied()
                .unwrap_or(self.opening_leaves);
            self.prepared.truncate_owned(len);
            self.boundaries = self.boundaries.prefix(shared);
        }
        for (offset, item) in items.iter_from(shared).enumerate() {
            let leaves = item_leaves_owned(item, shared + offset > 0);
            self.prepared.append_leaves(leaves.iter());
            self.boundaries = self.boundaries.append(&[self.prepared.len()]);
        }
        self.items = items.clone();
        self.opening = Some(opening.clone());
        let prefix = self.prepared.publish();
        let mut chunker = TextChunker::new(true);
        let mut suffix = Vec::new();
        for part in closing.parts() {
            match part {
                lash_protocol_types::llm::types::SegmentPart::Literal(text) => {
                    chunker.literal_owned(text, &mut suffix)
                }
                lash_protocol_types::llm::types::SegmentPart::Slot(slot) => {
                    suffix.push(encode_fragment(&RequestFragment::Attachment { slot }))
                }
            }
        }
        let mut inline: Vec<_> = suffix
            .iter()
            .map(|(reference, text)| (reference.blob.clone(), text.clone()))
            .collect();
        let refs: Vec<_> = suffix
            .iter()
            .map(|(reference, _)| reference.clone())
            .collect();
        let right = fragment_tree(&refs, &mut inline);
        let root = keep_fragment(
            &RequestFragment::Branch {
                left: prefix.clone(),
                right,
            },
            &mut inline,
        );
        let inline = inline
            .into_iter()
            .filter(|(hash, _)| self.prepared.texts.get(hash.as_str()).is_none())
            .map(|(hash, text)| (hash, text.text.to_string()))
            .collect();
        let texts = self.prepared.snapshot(&prefix);

        (
            ChunkedRequestTemplate {
                fetch_horizon: template.fetch_horizon,
                route: template.route.clone(),
                transport_stream: template.transport_stream(),
                generation: template.generation,
                wire_features: template.wire_features.clone(),
                root,
                prefix: Some(prefix),
                inline,
            },
            texts,
        )
    }
}

impl ChunkedRequestTemplate {
    /// Prepare the history prefix incrementally beside its first shared item.
    /// Cold preparation supplies the complete distinct fragment set. Reuse
    /// never assumes another call/session installed its bodies. The set shares
    /// every text with its prepared prefix and roots its exact closure.
    pub fn chunk(
        template: &lash_protocol_types::llm::types::RecordedRequestTemplate,
    ) -> (Self, RequestTexts) {
        Self::chunk_flat(template)
    }

    fn chunk_flat(
        template: &lash_protocol_types::llm::types::RecordedRequestTemplate,
    ) -> (Self, RequestTexts) {
        let mut leaves = Vec::new();
        let mut envelope = TextChunker::new(true);
        let mut items_end = None;
        let mut text_prefix_end = None;
        let mut previous_item = false;
        for segment in template.segment_iter() {
            if segment.is_item() {
                leaves.extend(item_leaves_owned(segment, previous_item));
                items_end = Some(leaves.len());
                previous_item = true;
            } else {
                previous_item = false;
                envelope.prefix_end = None;
                let start = leaves.len();
                let mut group = Vec::new();
                for part in segment.parts() {
                    match part {
                        lash_protocol_types::llm::types::SegmentPart::Literal(text) => {
                            envelope.literal_owned(text, &mut group)
                        }
                        lash_protocol_types::llm::types::SegmentPart::Slot(slot) => {
                            group.push(encode_fragment(&RequestFragment::Attachment { slot }))
                        }
                    }
                }
                if let Some(end) = envelope.prefix_end {
                    text_prefix_end = Some(start + end);
                }
                leaves.extend(group);
            }
        }
        let count = leaves.len();
        let prefix_end = items_end
            .or(text_prefix_end)
            .filter(|end| *end > 0 && *end < count);
        let mut prepared = PreparedPrefix::default();
        prepared.append_leaves(leaves[..prefix_end.unwrap_or(count)].iter());
        let tree = prepared.publish();
        let prefix = prefix_end.map(|_| tree.clone());
        let mut inline = Vec::new();
        let root = if let Some(end) = prefix_end {
            let fragments: Vec<_> = leaves[end..]
                .iter()
                .map(|(reference, encoded)| {
                    inline.push((reference.blob.clone(), encoded.clone()));
                    reference.clone()
                })
                .collect();
            let right = fragment_tree(&fragments, &mut inline);
            keep_fragment(
                &RequestFragment::Branch {
                    left: tree.clone(),
                    right,
                },
                &mut inline,
            )
        } else {
            tree.clone()
        };
        let inline = inline
            .into_iter()
            .filter(|(hash, _)| prepared.texts.get(hash.as_str()).is_none())
            .map(|(hash, fragment)| (hash, fragment.text.to_string()))
            .collect();
        let texts = prepared.snapshot(&tree);
        (
            Self {
                fetch_horizon: template.fetch_horizon,
                route: template.route.clone(),
                transport_stream: template.transport_stream(),
                generation: template.generation,
                wire_features: template.wire_features.clone(),
                root,
                prefix,
                inline,
            },
            texts,
        )
    }
}
