//! A single owner carries immutable prepared paths and allocation obligations.
use super::*;

/// Complete binary subtrees, ordered by decreasing size. Binary carries
/// create each internal node once; a bounded index publishes the forest.
#[derive(Clone, Debug, Default)]
struct PreparedPrefix {
    leaves: usize,
    forest: Option<std::sync::Arc<PreparedForest>>,
    root: Option<std::sync::Arc<AdmittedPromptText>>,
    pending: Vec<std::sync::Arc<AdmittedPromptText>>,
    texts: RequestTexts,
}
#[derive(Debug)]
struct PreparedForest {
    trees: [Option<PreparedTree>; usize::BITS as usize],
    _hold: AllocationHold,
}
#[derive(Clone, Debug)]
struct PreparedTree(std::sync::Arc<PreparedNode>);
#[derive(Debug)]
struct PreparedNode {
    body: std::sync::Arc<AdmittedPromptText>,
    leaves: usize,
    children: Option<[PreparedTree; 2]>,
    _hold: AllocationHold,
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
    fn branch_owned(
        left: Self,
        right: Self,
        prefix: &mut PreparedPrefix,
        budget: &AllocationBudget,
    ) -> Result<Self, AllocationError> {
        let hold = budget.reserve((std::mem::size_of::<PreparedNode>() + 16) as u64)?;
        let encoded = encode_branch_owned(&left.body, &right.body, budget)?;
        prefix.keep_body_owned(encoded.clone(), budget)?;
        Ok(Self(std::sync::Arc::new(PreparedNode {
            body: encoded,
            leaves: left.leaves + right.leaves,
            children: Some([left, right]),
            _hold: hold,
        })))
    }
    fn release(
        self,
        prefix: &mut PreparedPrefix,
        budget: &AllocationBudget,
    ) -> Result<(), AllocationError> {
        prefix.remove_owned(self.body.hash.as_str(), budget)?;
        if let Some(children) = &self.children {
            for child in children {
                child.clone().release(prefix, budget)?;
            }
        }
        Ok(())
    }
    fn retain_prefix(
        self,
        len: usize,
        prefix: &mut PreparedPrefix,
        budget: &AllocationBudget,
    ) -> Result<(), AllocationError> {
        if len == self.leaves {
            let height = self.leaves.ilog2() as usize;
            let forest = prefix.forest_mut(budget)?;
            debug_assert!(forest[height].is_none());
            forest[height] = Some(self);
        } else if len == 0 {
            self.release(prefix, budget)?;
        } else {
            prefix.remove_owned(self.body.hash.as_str(), budget)?;
            let [left, right] = self
                .children
                .as_ref()
                .expect("partial complete tree")
                .clone();
            let left_len = len.min(left.leaves);
            let right_len = len - left_len;
            left.retain_prefix(left_len, prefix, budget)?;
            right.retain_prefix(right_len, prefix, budget)?;
        }
        Ok(())
    }
}
impl PreparedPrefix {
    fn forest_mut(
        &mut self,
        budget: &AllocationBudget,
    ) -> Result<&mut [Option<PreparedTree>; usize::BITS as usize], AllocationError> {
        if self
            .forest
            .as_ref()
            .is_none_or(|forest| std::sync::Arc::strong_count(forest) > 1)
        {
            let hold = budget.reserve((std::mem::size_of::<PreparedForest>() + 16) as u64)?;
            let trees = self.forest.as_ref().map_or_else(
                || std::array::from_fn(|_| None),
                |forest| forest.trees.clone(),
            );
            self.forest = Some(std::sync::Arc::new(PreparedForest { trees, _hold: hold }));
        }
        Ok(
            &mut std::sync::Arc::get_mut(self.forest.as_mut().expect("private forest"))
                .expect("forest copy is private")
                .trees,
        )
    }
    fn unpublish(&mut self, budget: &AllocationBudget) -> Result<(), AllocationError> {
        if let Some(root) = self.root.take() {
            self.remove_owned(root.hash.as_str(), budget)?;
        }
        Ok(())
    }
    fn publish(&mut self, budget: &AllocationBudget) -> Result<PromptTextRef, AllocationError> {
        if self.root.is_none() {
            // The forest has at most one complete subtree per machine-word bit.
            let hold = budget.reserve(
                (usize::BITS as usize * (std::mem::size_of::<PromptTextRef>() + 64)) as u64,
            )?;
            let children = self
                .forest
                .as_ref()
                .into_iter()
                .flat_map(|forest| forest.trees.iter())
                .rev()
                .flatten()
                .map(PreparedTree::reference)
                .collect();
            let (_, root) = encode_fragment_owned(&RequestFragment::Index { children }, budget)?;
            drop(hold);
            self.keep_body_owned(root.clone(), budget)?;
            self.root = Some(root);
        }
        let root = self.root.as_ref().expect("published forest");
        Ok(PromptTextRef {
            blob: BlobRef(root.hash.clone()),
            bytes: u32::try_from(root.text.len()).unwrap_or(u32::MAX),
        })
    }
    fn snapshot(&self, root: &PromptTextRef) -> RequestTexts {
        RequestTexts {
            roots: vec![root.blob.as_str().to_owned()],
            texts: self.texts.texts.clone(),
            len: self.texts.len,
            delta: None,
            _metadata: Default::default(),
        }
    }
    fn keep_body_owned(
        &mut self,
        encoded: std::sync::Arc<AdmittedPromptText>,
        budget: &AllocationBudget,
    ) -> Result<(), AllocationError> {
        let count = self
            .texts
            .texts
            .get(encoded.hash.as_str())
            .map_or(0, |entry| entry.count);
        if count == 0 {
            self.pending.push(encoded.clone());
            self.texts.len += 1;
        }
        self.texts.texts = std::mem::take(&mut self.texts.texts).with_insert(
            TextKey(encoded.clone()),
            TextUse {
                text: encoded,
                count: count + 1,
            },
            budget,
            |_, _| 0,
        )?;
        Ok(())
    }
    fn keep_owned(
        &mut self,
        _reference: &PromptTextRef,
        encoded: std::sync::Arc<AdmittedPromptText>,
        budget: &AllocationBudget,
    ) -> Result<(), AllocationError> {
        self.keep_body_owned(encoded, budget)
    }
    fn remove_owned(
        &mut self,
        hash: &str,
        budget: &AllocationBudget,
    ) -> Result<(), AllocationError> {
        if let Some(entry) = self.texts.texts.get(hash).cloned() {
            if entry.count == 1 {
                self.texts.texts = std::mem::take(&mut self.texts.texts).without(hash, budget)?;
                self.texts.len -= 1;
            } else {
                self.texts.texts = std::mem::take(&mut self.texts.texts).with_insert(
                    TextKey(entry.text.clone()),
                    TextUse {
                        count: entry.count - 1,
                        ..entry
                    },
                    budget,
                    |_, _| 0,
                )?;
            }
        }
        Ok(())
    }
    fn truncate_owned(
        &mut self,
        len: usize,
        budget: &AllocationBudget,
    ) -> Result<(), AllocationError> {
        if len == self.leaves {
            return Ok(());
        }
        self.unpublish(budget)?;
        let forest = self.forest.take();
        let mut remaining = len;
        for tree in forest
            .iter()
            .flat_map(|forest| forest.trees.iter())
            .rev()
            .flatten()
            .cloned()
        {
            let keep = remaining.min(tree.leaves);
            remaining -= keep;
            tree.retain_prefix(keep, self, budget)?;
        }
        self.leaves = len;
        Ok(())
    }
    fn append_leaves<'a>(
        &mut self,
        leaves: impl Iterator<Item = &'a Leaf>,
        budget: &AllocationBudget,
    ) -> Result<(), AllocationError> {
        for (reference, encoded) in leaves {
            self.keep_owned(reference, encoded.clone(), budget)?;
            self.leaves += 1;
            let hold = budget.reserve(
                (std::mem::size_of::<PreparedNode>() + 16 + reference.blob.as_str().len()) as u64,
            )?;
            let leaf = PreparedTree(std::sync::Arc::new(PreparedNode {
                body: encoded.clone(),
                leaves: 1,
                children: None,
                _hold: hold,
            }));
            self.unpublish(budget)?;
            let mut tree = leaf;
            let mut height = 0;
            while let Some(left) = self.forest_mut(budget)?[height].take() {
                tree = PreparedTree::branch_owned(left, tree, self, budget)?;
                height += 1;
            }
            self.forest_mut(budget)?[height] = Some(tree);
        }
        Ok(())
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
    /// Lowered request nodes have the same claim and frame lifetime as the prompt.
    pub fn lowering_cache(&self) -> &lash_protocol_types::llm::types::SegmentCache {
        &self.lowering
    }
    /// A preparation edits only its private lowering directory; its immutable
    /// source, rendered items and fragment paths remain shared with readers.
    pub fn draft(&self, budget: &AllocationBudget) -> Result<Self, AllocationError> {
        let lowering = self.lowering.draft(budget)?;
        Ok(Self {
            lowering,
            ..self.clone()
        })
    }
    #[cfg(test)]
    pub(super) fn retained_root(&self) -> Option<PromptTextRef> {
        self.prepared.root.as_ref().map(|root| PromptTextRef {
            blob: BlobRef(root.hash.clone()),
            bytes: u32::try_from(root.text.len()).unwrap_or(u32::MAX),
        })
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
        budget: &AllocationBudget,
    ) -> Result<(ChunkedRequestTemplate, RequestTexts), AllocationError> {
        let bound = template.retained_items().map_or_else(
            || {
                template.literal_len().saturating_mul(16).saturating_add(
                    template
                        .segment_iter()
                        .map(|segment| segment.parts().len().saturating_mul(4096))
                        .sum::<usize>(),
                )
            },
            |(opening, _, closing)| {
                opening
                    .literal_len()
                    .saturating_add(closing.literal_len())
                    .saturating_mul(8)
            },
        );
        let metadata = budget.reserve(bound.saturating_add(4096) as u64)?;
        let mut draft = self.clone();
        let (body, mut texts) = draft.chunk_draft(template, budget)?;
        texts.retain_allocation(metadata);
        *self = draft;
        Ok((body, texts))
    }
    fn chunk_draft(
        &mut self,
        template: &lash_protocol_types::llm::types::RecordedRequestTemplate,
        budget: &AllocationBudget,
    ) -> Result<(ChunkedRequestTemplate, RequestTexts), AllocationError> {
        let Some((opening, items, closing)) = template.retained_items() else {
            self.clear();
            return ChunkedRequestTemplate::chunk_owned(template, budget);
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
                        chunker.literal_owned(text, &mut leaves, budget)?
                    }
                    lash_protocol_types::llm::types::SegmentPart::Slot(slot) => leaves.push(
                        encode_fragment_owned(&RequestFragment::Attachment { slot }, budget)?,
                    ),
                }
            }
            self.prepared.append_leaves(leaves.iter(), budget)?;
            self.opening_leaves = self.prepared.leaves;
        } else if shared < self.items.len() {
            let len = shared
                .checked_sub(1)
                .and_then(|index| self.boundaries.get(index))
                .copied()
                .unwrap_or(self.opening_leaves);
            self.prepared.truncate_owned(len, budget)?;
            self.boundaries = self.boundaries.prefix(shared, budget)?;
        }
        for (offset, item) in items.iter_from(shared).enumerate() {
            let leaves = item_leaves_owned(item, shared + offset > 0, budget)?;
            self.prepared.append_leaves(leaves.iter(), budget)?;
            self.boundaries = self.boundaries.append(&[self.prepared.leaves], budget)?;
        }
        self.items = items.clone();
        self.opening = Some(opening.clone());
        let prefix = self.prepared.publish(budget)?;
        let mut chunker = TextChunker::new(true);
        let mut suffix = Vec::new();
        for part in closing.parts() {
            match part {
                lash_protocol_types::llm::types::SegmentPart::Literal(text) => {
                    chunker.literal_owned(text, &mut suffix, budget)?
                }
                lash_protocol_types::llm::types::SegmentPart::Slot(slot) => suffix.push(
                    encode_fragment_owned(&RequestFragment::Attachment { slot }, budget)?,
                ),
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
        let delta_hold = budget.reserve(
            (self.prepared.pending.len()
                * std::mem::size_of::<std::sync::Arc<AdmittedPromptText>>()
                + 128) as u64,
        )?;
        texts.retain_allocation(delta_hold);
        texts.delta = Some(
            std::mem::take(&mut self.prepared.pending)
                .into_iter()
                .filter(|text| self.prepared.texts.get(text.hash.as_str()).is_some())
                .collect::<Vec<_>>()
                .into(),
        );
        Ok((
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
        ))
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
        Self::chunk_owned(template, &AllocationBudget::unowned())
            .expect("independent flat chunking")
    }

    fn chunk_owned(
        template: &lash_protocol_types::llm::types::RecordedRequestTemplate,
        budget: &AllocationBudget,
    ) -> Result<(Self, RequestTexts), AllocationError> {
        let bound = template
            .literal_len()
            .saturating_mul(16)
            .saturating_add(
                template
                    .segment_iter()
                    .map(|segment| segment.parts().len().saturating_mul(4096))
                    .sum::<usize>(),
            )
            .saturating_add(4096);
        let metadata = budget.reserve(bound as u64)?;
        let mut leaves = Vec::new();
        let mut envelope = TextChunker::new(true);
        let mut items_end = None;
        let mut text_prefix_end = None;
        let mut previous_item = false;
        for segment in template.segment_iter() {
            if segment.is_item() {
                leaves.extend(item_leaves_owned(segment, previous_item, budget)?);
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
                            envelope.literal_owned(text, &mut group, budget)?
                        }
                        lash_protocol_types::llm::types::SegmentPart::Slot(slot) => group.push(
                            encode_fragment_owned(&RequestFragment::Attachment { slot }, budget)?,
                        ),
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
        prepared.append_leaves(leaves[..prefix_end.unwrap_or(count)].iter(), budget)?;
        let tree = prepared.publish(budget)?;
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
        let mut texts = prepared.snapshot(&tree);
        texts.retain_allocation(metadata);
        Ok((
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
        ))
    }
}
