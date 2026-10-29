//! One canonical nested-tree representation, independent of the cell kind.
use super::*;

impl<T: Cell> Tree<T> {
    pub fn children(&self, out: &mut Vec<PageRef>) {
        if let Some(root) = &self.root {
            match &root.link {
                Link::Sealed(page) => out.push(*page),
                Link::Inline { children, .. } => out.extend(children),
                Link::Dirty(_) => (),
            }
        }
    }

    /// The tree is empty or one inline leaf that references no page: a
    /// value that holds nothing a retirement could take away.
    pub fn is_self_contained(&self) -> bool {
        self.root.as_ref().is_none_or(
            |root| matches!(&root.link, Link::Inline { children, .. } if children.is_empty()),
        )
    }

    pub fn grants(&mut self, children: &[PageRef]) -> Result<(), HeapFault> {
        let Some(root) = &mut self.root else {
            return Ok(());
        };
        let root = Rc::make_mut(root);
        if let Link::Inline {
            body: Inline::Tokens(tokens),
            ..
        } = &root.link
        {
            let mut refs = Vec::new();
            Self::read_inline(
                root.family,
                root.summary,
                TokenReader::new(tokens),
                children,
                true,
                &mut |_, cell| {
                    cell.children(&mut refs);
                },
            )?;
            refs.sort_by_key(|p| p.id);
            refs.dedup_by_key(|p| p.id);
            let Link::Inline { children: own, .. } = &mut root.link else {
                unreachable!()
            };
            *own = refs;
            return Ok(());
        }
        match &mut root.link {
            Link::Sealed(page) => grant(page, children),
            Link::Inline {
                body: Inline::Rows(node) | Inline::Bound { rows: node, .. },
                children: own,
            } => {
                let rows = Rc::make_mut(node);
                let mut refs = Vec::new();
                for (_, cell) in rows {
                    cell.grants(children)?;
                    cell.children(&mut refs);
                }
                refs.sort_by_key(|p| p.id);
                refs.dedup_by_key(|p| p.id);
                *own = refs;
                Ok(())
            }
            Link::Inline {
                body: Inline::Tokens(_),
                ..
            } => unreachable!(),
            Link::Dirty(_) => Ok(()),
        }
    }

    pub(super) fn inline_node(
        family: Option<FamilyId>,
        expected: Summary,
        tokens: &[u8],
        children: &[PageRef],
        authenticate: bool,
    ) -> Result<Node<T>, HeapFault> {
        let mut rows = Vec::with_capacity(expected.len.min(KEYED_LEAF_ROWS) as usize);
        Self::read_inline(
            family,
            expected,
            TokenReader::new(tokens),
            children,
            authenticate,
            &mut |key, cell| {
                rows.push((Key::from(key), cell));
            },
        )?;
        Ok(Node::Leaf(rows))
    }

    fn inline_rows(
        family: Option<FamilyId>,
        expected: Summary,
        input: TokenReader<'_>,
    ) -> Result<Rc<[(Key, T)]>, HeapFault> {
        if expected.len == 0 || expected.len > KEYED_LEAF_ROWS {
            return Err(HeapFault::Malformed("invalid inline tree leaf size".into()));
        }
        let mut rows = InlineRows::new(expected.len as usize);
        Self::read_inline(family, expected, input, &[], false, &mut |key, cell| {
            rows.push((Key::from(key), cell));
        })?;
        Ok(rows.finish())
    }

    fn read_inline(
        family: Option<FamilyId>,
        expected: Summary,
        mut input: TokenReader<'_>,
        children: &[PageRef],
        authenticate: bool,
        visitor: &mut impl FnMut(&[u8], T),
    ) -> Result<(), HeapFault> {
        #[cfg(any(test, feature = "perf-witness"))]
        crate::perf_witness::record(|work| work.inline_decodes += 1);
        let invalid = |e: TokenError| HeapFault::Malformed(e.to_string());
        let count = input.seq().map_err(invalid)?;
        if count == 0 || count > KEYED_LEAF_ROWS || count != expected.len {
            return Err(HeapFault::Malformed("invalid inline tree leaf size".into()));
        }
        let mut previous = None;
        let mut summary = Summary::default();
        for _ in 0..count {
            input.expect_struct(2).map_err(invalid)?;
            let key = input.bytes().map_err(invalid)?;
            if previous.is_some_and(|last| last >= key) {
                return Err(HeapFault::Malformed(
                    "unsorted or duplicate tree keys".into(),
                ));
            }
            previous = Some(key);
            let mut cell = T::decode_cell(&mut input, family).map_err(invalid)?;
            if authenticate {
                cell.grants(children)?;
            }
            summary = Aggregate::of(&cell).replace(summary, None)?;
            visitor(key, cell);
        }
        input.finish().map_err(invalid)?;
        if summary != expected {
            return Err(HeapFault::Malformed(
                "inline tree aggregate mismatch".into(),
            ));
        }
        Ok(())
    }

    /// A fresh nested tree holding `rows`, as writing each into an empty
    /// tree in turn would: a later row replaces an earlier one with the same
    /// key. One that [`Self::seal_nested`] would seal inline is built in that
    /// form now, from the same prepared rows: a fresh object's small rows
    /// never become a working page that dirty pressure stages as a page of
    /// its own (FIG-6886). Any other is built as working leaves.
    pub fn nested_from_rows<K: Into<Key>>(
        rows: Vec<(K, T)>,
        sequence: bool,
        access: &Access<'_>,
    ) -> Result<Self, HeapFault> {
        let mut rows = keyed_rows(rows);
        if rows.is_empty() || rows.len() > KEYED_LEAF_ROWS as usize {
            return Self::from_keyed(rows, sequence, access);
        }
        let mut work = SealWork::new(SealBudget {
            pages: u64::MAX,
            bytes: u64::MAX,
        });
        let mut children = Vec::new();
        for (_, cell) in &mut rows {
            if !cell.prepare(access, &mut work)? {
                return Err(HeapFault::Malformed(
                    "unbounded preparation ran out of budget".into(),
                ));
            }
            cell.children(&mut children);
        }
        let encoded = access.encoded_len(&rows);
        if encoded > INLINE_TREE_BYTES {
            return Self::from_keyed(rows, sequence, access);
        }
        children.sort_by_key(|p| p.id);
        children.dedup_by_key(|p| p.id);
        let mut root = Branch {
            family: None,
            encoded_bytes: None,
            is_leaf: None,
            tasks_known: true,
            members_known: true,
            first: Key::from(&[][..]),
            summary: Summary::default(),
            link: Link::Inline {
                body: Inline::Rows(Rc::from(rows)),
                children,
            },
        };
        let Link::Inline {
            body: Inline::Rows(node),
            ..
        } = &root.link
        else {
            unreachable!()
        };
        let summary = Summary {
            len: node.len() as u64,
            cost: checked_sum(node.iter().map(|(_, c)| c.cost()))?,
            tasks: checked_sum(node.iter().map(|(_, c)| c.task_edges()))?,
            members: checked_sum(node.iter().map(|(_, c)| c.live_members()))?,
        };
        root.first = node[0].0.clone();
        root.summary = summary;
        root.encoded_bytes = Some(encoded as u64);
        root.is_leaf = Some(true);
        Ok(Self {
            root: Some(Rc::new(root)),
            sequence,
        })
    }

    /// A nested tree is inline exactly when all its rows encode as one leaf
    /// within [`INLINE_TREE_BYTES`], and otherwise canonical pages. External
    /// root descriptors use `seal`, which always publishes pages.
    pub fn seal_nested(
        &mut self,
        access: &Access<'_>,
        work: &mut SealWork,
    ) -> Result<bool, HeapFault> {
        loop {
            let Some(root) = &self.root else {
                return Ok(true);
            };
            match &root.link {
                Link::Inline { .. } => return Ok(true),
                Link::Sealed(_)
                    if root.is_leaf == Some(false)
                        || root
                            .encoded_bytes
                            .is_some_and(|n| n > INLINE_TREE_BYTES as u64) =>
                {
                    return Ok(true);
                }
                _ => (),
            }
            let node = Self::load(root, access, self.sequence)?;
            match node {
                // More rows than this never fit an inline body.
                Node::Index(_) if self.len() <= KEYED_LEAF_ROWS => {
                    let mut rows = Vec::new();
                    self.visit(access, &mut |key, cell| {
                        rows.push((Key::from(key), cell.clone()));
                        true
                    })?;
                    self.root = Some(Rc::new(dirty(Node::Leaf(rows), access, self.sequence)?));
                }
                Node::Index(_) => return self.seal(access, work),
                Node::Staged(..) => unreachable!("loaded staged node"),
                Node::Leaf(mut rows) => {
                    let mut children = Vec::new();
                    for (_, cell) in &mut rows {
                        if !cell.prepare(access, work)? {
                            return Ok(false);
                        }
                        cell.children(&mut children);
                    }
                    let encoded = access.encoded_len(&rows);
                    if encoded > INLINE_TREE_BYTES {
                        // Keep prepared children even if the next page exceeds
                        // this seal budget; retry never discards staged work.
                        let root = self.root.as_mut().ok_or_else(|| {
                            HeapFault::Malformed("nonempty inline tree lost its root".into())
                        })?;
                        let root = Rc::make_mut(root);
                        let node = Node::Leaf(rows);
                        let page = Rc::new(residency::DirtyNode::new(node));
                        access.track(&page, &page.borrow(), self.sequence);
                        root.link = Link::Dirty(page);
                        root.encoded_bytes = None;
                        root.is_leaf = None;
                        return self.seal(access, work);
                    }
                    children.sort_by_key(|p| p.id);
                    children.dedup_by_key(|p| p.id);
                    let root = self.root.as_mut().ok_or_else(|| {
                        HeapFault::Malformed("nonempty inline tree lost its root".into())
                    })?;
                    let root = Rc::make_mut(root);
                    root.link = Link::Inline {
                        body: if root.family.is_none() {
                            Inline::Rows(Rc::from(rows))
                        } else {
                            Inline::Tokens(encode_body(&rows).into())
                        },
                        children,
                    };
                    root.encoded_bytes = Some(encoded as u64);
                    root.is_leaf = Some(true);
                    return Ok(true);
                }
            }
        }
    }

    /// The frozen writer changes cell interpretation without changing its
    /// canonical relative tokens or duplicating its bounded storage links.
    pub(crate) fn retype<U: Cell>(self) -> Result<Tree<U>, HeapFault> {
        let root = self
            .root
            .map(|r| {
                let r = Rc::unwrap_or_clone(r);
                let link = match r.link {
                    Link::Sealed(p) => Link::Sealed(p),
                    Link::Inline { body, children } => Link::Inline {
                        body: Inline::Tokens(body.tokens()),
                        children,
                    },
                    Link::Dirty(_) => {
                        return Err(HeapFault::Malformed("unsealed tree retype".into()));
                    }
                };
                Ok(Branch {
                    family: r.family,
                    encoded_bytes: r.encoded_bytes,
                    is_leaf: r.is_leaf,
                    first: r.first,
                    summary: r.summary,
                    tasks_known: r.tasks_known,
                    members_known: r.members_known,
                    link,
                })
            })
            .transpose()?;
        Ok(Tree {
            root: root.map(Rc::new),
            sequence: self.sequence,
        })
    }
}
impl<T: Cell> TokenCodec for Tree<T> {
    fn encode(&self, out: &mut TokenWriter) {
        out.seq(7);
        self.sequence.encode(out);
        self.root.as_ref().and_then(|r| r.family).encode(out);
        match &self.root {
            None => out.sum(0, 0),
            Some(r) => match &r.link {
                // The leaf's own tokens, not an opaque blob: each reference
                // in it is one the enclosing page's envelope declares.
                Link::Inline { body, .. } => {
                    out.sum(1, 1);
                    body.encode(out);
                }
                Link::Sealed(page) => {
                    out.sum(2, 3);
                    page.encode(out);
                    r.encoded_bytes.unwrap_or(u64::MAX).encode(out);
                    r.is_leaf.unwrap_or(true).encode(out);
                }
                // Packing estimates only. Preparing the containing record
                // replaces this placeholder before publication.
                Link::Dirty(_) => {
                    out.sum(2, 3);
                    PageRef {
                        id: PageId([0; 32]),
                        grant: lash_kernel_doc::heap::PageGrant::NONE,
                    }
                    .encode(out);
                    u64::MAX.encode(out);
                    true.encode(out);
                }
            },
        }
        self.len().encode(out);
        self.cost().encode(out);
        self.task_edges().encode(out);
        self.root
            .as_ref()
            .map_or(0, |r| r.summary.members)
            .encode(out);
    }
    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        input.expect_struct(7)?;
        let sequence = bool::decode(input)?;
        let family = Option::<FamilyId>::decode(input)?;
        let (tag, fields) = input.sum()?;
        let mut inline_tokens = None;
        let (link, encoded_bytes, is_leaf) = match tag {
            0 => {
                TokenReader::expect_fields(fields, 0)?;
                (None, None, None)
            }
            1 => {
                TokenReader::expect_fields(fields, 1)?;
                let tokens = input.tree()?;
                if tokens.len() > INLINE_TREE_BYTES {
                    return Err(TokenError::Invalid("oversized inline tree".into()));
                }
                inline_tokens = Some(tokens);
                (None, Some(tokens.len() as u64), Some(true))
            }
            2 => {
                TokenReader::expect_fields(fields, 3)?;
                let page = PageRef::decode(input)?;
                let bytes = u64::decode(input)?;
                let leaf = bool::decode(input)?;
                if (leaf && bytes <= INLINE_TREE_BYTES as u64) || bytes == u64::MAX {
                    return Err(TokenError::Invalid("noncanonical paged nested tree".into()));
                }
                (Some(Link::Sealed(page)), Some(bytes), Some(leaf))
            }
            _ => return Err(TokenError::UnsupportedVariant { variant: tag }),
        };
        let summary = Summary {
            len: u64::decode(input)?,
            cost: u64::decode(input)?,
            tasks: u64::decode(input)?,
            members: u64::decode(input)?,
        };
        if (link.is_some() || inline_tokens.is_some()) != (summary.len != 0)
            || (summary.len == 0 && summary != Summary::default())
        {
            return Err(TokenError::Invalid("tree root length mismatch".into()));
        }
        let link = if let Some(tokens) = inline_tokens {
            let body = if family.is_none() {
                Inline::Rows(
                    Self::inline_rows(family, summary, input.subreader(tokens))
                        .map_err(|e| TokenError::Invalid(e.to_string()))?,
                )
            } else {
                Inline::Bound {
                    tokens: Arc::from(tokens),
                    rows: Self::inline_rows(family, summary, input.subreader(tokens))
                        .map_err(|e| TokenError::Invalid(e.to_string()))?,
                }
            };
            Some(Link::Inline {
                body,
                children: Vec::new(),
            })
        } else {
            link
        };
        let root = link.map(|link| Branch {
            family,
            encoded_bytes,
            is_leaf,
            first: Key::from(&[][..]),
            summary,
            tasks_known: true,
            members_known: true,
            link,
        });
        Ok(Self {
            root: root.map(Rc::new),
            sequence,
        })
    }
}

/// Owns the initialized prefix even when decoding refuses a later row.
struct InlineRows<T> {
    rows: Option<Rc<[std::mem::MaybeUninit<T>]>>,
    initialized: usize,
}
#[expect(
    clippy::expect_used,
    reason = "the private builder owns one unshared allocation until finish takes it"
)]
impl<T> InlineRows<T> {
    fn new(len: usize) -> Self {
        Self {
            rows: Some(Rc::new_uninit_slice(len)),
            initialized: 0,
        }
    }
    fn push(&mut self, row: T) {
        Rc::get_mut(self.rows.as_mut().expect("private inline rows")).expect("private inline rows")
            [self.initialized]
            .write(row);
        self.initialized += 1;
    }
    #[expect(
        unsafe_code,
        reason = "every element was initialized before sharing the slice"
    )]
    fn finish(mut self) -> Rc<[T]> {
        assert_eq!(
            self.initialized,
            self.rows.as_ref().expect("private inline rows").len()
        );
        let rows = self.rows.take().expect("private inline rows");
        self.initialized = 0;
        // SAFETY: push initialized exactly rows.len() elements and the
        // allocation has never been shared. Drop no longer owns the prefix.
        unsafe { rows.assume_init() }
    }
}
#[expect(
    clippy::expect_used,
    reason = "an unfinished builder still owns its unique allocation"
)]
impl<T> Drop for InlineRows<T> {
    #[expect(
        unsafe_code,
        reason = "the initialized prefix owns exactly these elements"
    )]
    fn drop(&mut self) {
        let Some(rows) = &mut self.rows else {
            return;
        };
        for row in &mut Rc::get_mut(rows).expect("private inline rows")[..self.initialized] {
            // SAFETY: only push advances initialized, after writing the row.
            unsafe { row.assume_init_drop() };
        }
    }
}
