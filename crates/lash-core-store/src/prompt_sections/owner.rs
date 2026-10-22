//! A single owner carries immutable prepared prompt paths.
use super::*;

/// Complete binary subtrees, ordered by decreasing size. Binary carries
/// create each internal node once; chronological links share older tails.
#[derive(Clone, Debug, Default)]
struct PreparedPrefix {
    leaves: usize,
    forest: Option<std::sync::Arc<PreparedForest>>,
    root: Option<std::sync::Arc<PromptText>>,
    pending: Vec<std::sync::Arc<PromptText>>,
    texts: RequestTexts,
}
#[derive(Debug)]
struct PreparedForest {
    trees: [Option<PreparedMountain>; usize::BITS as usize],
}
/// A complete mountain and the chronological prefix through that mountain.
/// The prefix is either this mountain or Index [older prefix, mountain].
#[derive(Clone, Debug)]
struct PreparedMountain {
    tree: PreparedTree,
    prefix_root: std::sync::Arc<PromptText>,
}
impl PreparedMountain {
    fn has_link(&self) -> bool {
        !std::sync::Arc::ptr_eq(&self.prefix_root, &self.tree.body)
    }
}
#[derive(Clone, Debug)]
struct PreparedTree(std::sync::Arc<PreparedNode>);
#[derive(Debug)]
struct PreparedNode {
    body: std::sync::Arc<PromptText>,
    leaves: usize,
    children: Option<[PreparedTree; 2]>,
}
impl std::ops::Deref for PreparedTree {
    type Target = PreparedNode;
    fn deref(&self) -> &PreparedNode {
        &self.0
    }
}
impl PreparedTree {
    fn reference(&self) -> PromptTextRef {
        PromptTextRef {
            blob: BlobRef(self.body.hash.clone()),
            bytes: u32::try_from(self.body.text.len()).unwrap_or(u32::MAX),
        }
    }
    fn branch_owned(left: Self, right: Self, prefix: &mut PreparedPrefix) -> Self {
        let encoded = encode_branch_owned(&left.body, &right.body);
        prefix.keep_body_owned(encoded.clone());
        Self(std::sync::Arc::new(PreparedNode {
            body: encoded,
            leaves: left.leaves + right.leaves,
            children: Some([left, right]),
        }))
    }
    fn release(self, prefix: &mut PreparedPrefix) {
        prefix.remove_owned(self.body.hash.as_str());
        if let Some(children) = &self.children {
            for child in children {
                child.clone().release(prefix);
            }
        }
    }
    fn retain_prefix(self, len: usize, prefix: &mut PreparedPrefix) {
        if len == self.leaves {
            let height = self.leaves.ilog2() as usize;
            let forest = prefix.forest_mut();
            debug_assert!(forest[height].is_none());
            forest[height] = Some(PreparedMountain {
                prefix_root: self.body.clone(),
                tree: self,
            });
        } else if len == 0 {
            self.release(prefix);
        } else {
            prefix.remove_owned(self.body.hash.as_str());
            let [left, right] = self
                .children
                .as_ref()
                .expect("partial complete tree")
                .clone();
            let left_len = len.min(left.leaves);
            let right_len = len - left_len;
            left.retain_prefix(left_len, prefix);
            right.retain_prefix(right_len, prefix);
        }
    }
}
impl PreparedPrefix {
    fn forest_mut(&mut self) -> &mut [Option<PreparedMountain>; usize::BITS as usize] {
        if self
            .forest
            .as_ref()
            .is_none_or(|forest| std::sync::Arc::strong_count(forest) > 1)
        {
            let trees = self.forest.as_ref().map_or_else(
                || std::array::from_fn(|_| None),
                |forest| forest.trees.clone(),
            );
            self.forest = Some(std::sync::Arc::new(PreparedForest { trees }));
        }

        &mut std::sync::Arc::get_mut(self.forest.as_mut().expect("private forest"))
            .expect("forest copy is private")
            .trees
    }
    fn unpublish(&mut self) {
        // Nonempty roots belong to their mountain slots. Clearing this
        // borrowed publication does not retire a shared older tail.
        if let Some(root) = self.root.take()
            && self.leaves == 0
        {
            self.remove_owned(root.hash.as_str());
        }
    }
    fn link_owned(
        &mut self,
        older: Option<&std::sync::Arc<PromptText>>,
        tree: &PreparedTree,
    ) -> std::sync::Arc<PromptText> {
        let Some(older) = older else {
            return tree.body.clone();
        };

        let children = vec![
            PromptTextRef {
                blob: BlobRef(older.hash.clone()),
                bytes: u32::try_from(older.text.len()).unwrap_or(u32::MAX),
            },
            tree.reference(),
        ];
        let (_, link) = encode_fragment(&RequestFragment::Index { children });
        self.keep_body_owned(link.clone());
        link
    }
    fn publish(&mut self) -> PromptTextRef {
        if self.root.is_none() {
            let root = self
                .forest
                .as_ref()
                .and_then(|forest| forest.trees.iter().flatten().next())
                .map(|mountain| mountain.prefix_root.clone());
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
        let root = self.root.as_ref().expect("published forest");
        PromptTextRef {
            blob: BlobRef(root.hash.clone()),
            bytes: u32::try_from(root.text.len()).unwrap_or(u32::MAX),
        }
    }
    fn snapshot(&self, root: &PromptTextRef) -> RequestTexts {
        RequestTexts {
            roots: vec![root.blob.as_str().to_owned()],
            texts: self.texts.texts.clone(),
            len: self.texts.len,
            delta: None,
        }
    }
    fn keep_body_owned(&mut self, encoded: std::sync::Arc<PromptText>) {
        let count = self
            .texts
            .texts
            .get(encoded.hash.as_str())
            .map_or(0, |entry| entry.count);
        if count == 0 {
            self.pending.push(encoded.clone());
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
    fn keep_owned(&mut self, _reference: &PromptTextRef, encoded: std::sync::Arc<PromptText>) {
        self.keep_body_owned(encoded)
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
        if len == self.leaves {
            return;
        }
        self.unpublish();
        let forest = self.forest.take();
        // Discard old publication links, keeping complete mountain bodies
        // until retain_prefix determines their exact surviving ownership.
        for mountain in forest
            .iter()
            .flat_map(|forest| forest.trees.iter())
            .flatten()
        {
            if mountain.has_link() {
                self.remove_owned(mountain.prefix_root.hash.as_str());
            }
        }
        let mut remaining = len;
        for mountain in forest
            .iter()
            .flat_map(|forest| forest.trees.iter())
            .rev()
            .flatten()
        {
            let keep = remaining.min(mountain.tree.leaves);
            remaining -= keep;
            mountain.tree.clone().retain_prefix(keep, self);
        }
        // Rebuild only the surviving chronological stack, oldest first.
        let mut older = None;
        for height in (0..usize::BITS as usize).rev() {
            let tree = self
                .forest
                .as_ref()
                .and_then(|forest| forest.trees[height].as_ref())
                .map(|mountain| mountain.tree.clone());
            if let Some(tree) = tree {
                let prefix_root = self.link_owned(older.as_ref(), &tree);
                self.forest_mut()[height] = Some(PreparedMountain {
                    tree,
                    prefix_root: prefix_root.clone(),
                });
                older = Some(prefix_root);
            }
        }
        self.leaves = len;
    }
    fn append_leaves<'a>(&mut self, leaves: impl Iterator<Item = &'a Leaf>) {
        for (reference, encoded) in leaves {
            self.unpublish();
            self.keep_owned(reference, encoded.clone());
            self.leaves += 1;

            let leaf = PreparedTree(std::sync::Arc::new(PreparedNode {
                body: encoded.clone(),
                leaves: 1,
                children: None,
            }));
            let mut tree = leaf;
            let mut height = 0;
            while let Some(left) = self.forest_mut()[height].take() {
                // The mountain survives as a carry child. Only its former
                // chronological publication link becomes obsolete.
                if left.has_link() {
                    self.remove_owned(left.prefix_root.hash.as_str());
                }
                tree = PreparedTree::branch_owned(left.tree, tree, self);
                height += 1;
            }
            let older = self
                .forest
                .as_ref()
                .and_then(|forest| forest.trees[height + 1..].iter().flatten().next())
                .map(|mountain| mountain.prefix_root.clone());
            let prefix_root = self.link_owned(older.as_ref(), &tree);
            self.forest_mut()[height] = Some(PreparedMountain { tree, prefix_root });
        }
    }
}

/// One acknowledged prompt draft; readers share immutable paths and obligations.
#[derive(Clone, Debug, Default)]
pub struct OwnerPromptState {
    lowering: lash_protocol_types::llm::types::SegmentCache,
    items: lash_protocol_types::llm::types::RequestItems,
    opening: Option<lash_protocol_types::llm::types::RequestSegment>,
    prepared: PreparedPrefix,
    boundaries: lash_protocol_types::persistent_sequence::PersistentSequence<usize>,
    opening_leaves: usize,
}
impl OwnerPromptState {
    /// Observe the exact prepared forest without keeping its source alive.
    #[cfg(any(test, feature = "testing"))]
    pub fn prepared_source_for_testing(&self) -> Option<std::sync::Weak<dyn Send + Sync>> {
        let source: std::sync::Arc<dyn Send + Sync> = self.prepared.forest.as_ref()?.clone();
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
        self.prepared.leaves
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
        self.prepared.pending = Vec::with_capacity(usize::BITS as usize);
        let opening_same = self.opening.as_ref().is_some_and(|prior| prior == opening);
        let shared = if opening_same {
            self.items.shared_prefix(items)
        } else {
            0
        };
        if !opening_same {
            self.prepared = PreparedPrefix::default();
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
            self.opening_leaves = self.prepared.leaves;
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
            self.boundaries = self.boundaries.append(&[self.prepared.leaves]);
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
        let mut texts = self.prepared.snapshot(&prefix);

        texts.delta = Some(
            std::mem::take(&mut self.prepared.pending)
                .into_iter()
                .filter(|text| self.prepared.texts.get(text.hash.as_str()).is_some())
                .collect::<Vec<_>>()
                .into(),
        );
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
    /// A call supplies the complete distinct fragment set even on a cache hit:
    /// reuse never assumes that a different call/session installed its bodies.
    /// The set is the carried prefix tree's, rooted at that tree's root, and
    /// shares every text with it.
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
