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

    pub fn grants(&mut self, children: &[PageRef]) -> Result<(), HeapFault> {
        let Some(root) = &mut self.root else {
            return Ok(());
        };
        match &root.link {
            Link::Sealed(_) => {
                let Link::Sealed(page) = &mut root.link else {
                    unreachable!()
                };
                grant(page, children)
            }
            Link::Inline { tokens, .. } => {
                // Decode only this bounded body. Keep its references, never a
                // second copy of the enclosing page's entire child table.
                let mut node = Self::inline_node(root, tokens, children, true)?;
                let Node::Leaf(rows) = &mut node else {
                    unreachable!()
                };
                let mut refs = Vec::new();
                for (_, cell) in rows {
                    cell.children(&mut refs);
                }
                refs.sort_by_key(|p| p.id);
                refs.dedup_by_key(|p| p.id);
                let Link::Inline { children: own, .. } = &mut root.link else {
                    unreachable!()
                };
                *own = refs;
                Ok(())
            }
            Link::Dirty(_) => Ok(()),
        }
    }

    pub(super) fn inline_node(
        branch: &Branch<T>,
        tokens: &[u8],
        children: &[PageRef],
        authenticate: bool,
    ) -> Result<Node<T>, HeapFault> {
        let invalid = |e: TokenError| HeapFault::Malformed(e.to_string());
        let mut input = TokenReader::new(tokens);
        let count = input.seq().map_err(invalid)?;
        if count == 0 || count > KEYED_LEAF_ROWS {
            return Err(HeapFault::Malformed("invalid inline tree leaf size".into()));
        }
        let mut rows = Vec::new();
        for _ in 0..count {
            input.expect_struct(2).map_err(invalid)?;
            let key = Key::decode(&mut input).map_err(invalid)?;
            let mut cell = T::decode_cell(&mut input, branch.family).map_err(invalid)?;
            if authenticate {
                cell.grants(children)?;
            }
            rows.push((key, cell));
        }
        input.finish().map_err(invalid)?;
        check_keys(rows.iter().map(|r| r.0.as_slice()))
            .map_err(|e| HeapFault::Malformed(e.into()))?;
        let node = Node::Leaf(rows);
        let mut actual = branch.clone();
        refresh(&mut actual, &node)?;
        if actual.summary != branch.summary {
            return Err(HeapFault::Malformed(
                "inline tree aggregate mismatch".into(),
            ));
        }
        Ok(node)
    }

    /// Nested records always inline a small leaf. External root descriptors
    /// use `seal`, which always publishes a page.
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
                Node::Index(mut rows) if rows.len() == 1 => {
                    self.root = rows.pop();
                }
                Node::Index(_) if self.len() <= KEYED_LEAF_ROWS => {
                    let mut rows = Vec::new();
                    self.visit(access, &mut |key, cell| {
                        rows.push((Key(key.to_vec()), cell.clone()));
                        true
                    })?;
                    self.root = Some(dirty(Node::Leaf(rows))?);
                }
                Node::Index(_) => return self.seal(access, work),
                Node::Leaf(mut rows) => {
                    let mut children = Vec::new();
                    for (_, cell) in &mut rows {
                        if !cell.prepare(access, work)? {
                            return Ok(false);
                        }
                        cell.children(&mut children);
                    }
                    let tokens = encode_body(&rows);
                    if tokens.len() > INLINE_TREE_BYTES {
                        // Keep prepared children even if the next page exceeds
                        // this seal budget; retry never discards staged work.
                        let root = self.root.as_mut().expect("nonempty tree");
                        root.link = Link::Dirty(Rc::new(RefCell::new(Node::Leaf(rows))));
                        root.encoded_bytes = None;
                        root.is_leaf = None;
                        return self.seal(access, work);
                    }
                    children.sort_by_key(|p| p.id);
                    children.dedup_by_key(|p| p.id);
                    let root = self.root.as_mut().expect("nonempty tree");
                    root.link = Link::Inline {
                        tokens: tokens.into(),
                        children,
                    };
                    root.encoded_bytes = Some(root.link_tokens_len());
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
                let link = match r.link {
                    Link::Sealed(p) => Link::Sealed(p),
                    Link::Inline { tokens, children } => Link::Inline { tokens, children },
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
            root,
            sequence: self.sequence,
        })
    }
}
impl<T: Cell> Branch<T> {
    fn link_tokens_len(&self) -> u64 {
        match &self.link {
            Link::Inline { tokens, .. } => tokens.len() as u64,
            _ => unreachable!(),
        }
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
                Link::Inline { tokens, .. } => {
                    out.sum(1, 1);
                    out.bytes(tokens);
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
        let (link, encoded_bytes, is_leaf) = match tag {
            0 => {
                TokenReader::expect_fields(fields, 0)?;
                (None, None, None)
            }
            1 => {
                TokenReader::expect_fields(fields, 1)?;
                let tokens = input.bytes()?;
                if tokens.len() > INLINE_TREE_BYTES {
                    return Err(TokenError::Invalid("oversized inline tree".into()));
                }
                (
                    Some(Link::Inline {
                        tokens: Arc::from(tokens),
                        children: Vec::new(),
                    }),
                    Some(tokens.len() as u64),
                    Some(true),
                )
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
        if link.is_some() != (summary.len != 0)
            || (summary.len == 0 && summary != Summary::default())
        {
            return Err(TokenError::Invalid("tree root length mismatch".into()));
        }
        let root = link.map(|link| Branch {
            family,
            encoded_bytes,
            is_leaf,
            first: Key(Vec::new()),
            summary,
            tasks_known: true,
            members_known: true,
            link,
        });
        if let Some(r) = &root
            && let Link::Inline { tokens, .. } = &r.link
        {
            Self::inline_node(r, tokens, &[], false)
                .map_err(|e| TokenError::Invalid(e.to_string()))?;
        }
        Ok(Self { root, sequence })
    }
}
