//! Bounded semantic metadata; ancestry shares immutable parent nodes.
use super::directory::ordinal_key;
use super::payload::{build, visit};
use super::tree::{Access, Cell, SealWork, Tree, grant};
use super::{PageKind, SealBudget};
use lash_kernel_doc::heap::*;
use lash_kernel_doc::heap_values::{key_digest, text_summary};
use lash_kernel_value::names::FunctionId;
use lash_kernel_value::names::Name;
use lash_kernel_value::sites::Site;
use lash_kernel_value::sites::Unit;
use std::collections::BTreeSet;
use std::ops::ControlFlow;
use std::sync::{Arc, OnceLock};
use {lash_kernel_value::sites::SpawnIdentity, lash_kernel_value::sites::TaskIdentity};
use {
    lash_kernel_value::tokens::TokenCodec, lash_kernel_value::tokens::TokenError,
    lash_kernel_value::tokens::TokenReader, lash_kernel_value::tokens::TokenWriter,
    lash_kernel_value::tokens::encode_body,
};

fn budget() -> SealWork {
    SealWork::new(SealBudget {
        pages: u64::MAX,
        bytes: u64::MAX,
    })
}

/// Metadata keys retain canonical tokens up to the shared inline threshold.
/// Longer keys use the same domain-separated digest primitive as NormalKey.
pub fn metadata_key<T: TokenCodec>(value: &T) -> Vec<u8> {
    let tokens = encode_body(value);
    if tokens.len() <= super::tree::INLINE_TREE_BYTES {
        [vec![0], tokens].concat()
    } else {
        [vec![1], key_digest(&tokens).to_vec()].concat()
    }
}

#[derive(Clone, Debug)]
pub struct NameRef {
    text: TextRef,
    cached: Arc<OnceLock<Name>>,
    key: Arc<OnceLock<Vec<u8>>>,
}
impl NameRef {
    pub fn new(name: &Name, access: &Access<'_>) -> Result<Self, HeapFault> {
        if let Some(value) = access.metadata_name(name) {
            return Ok(value);
        }
        let text = name.as_str();
        let body = if text.len() as u64 <= IMMUTABLE_INLINE_MAX {
            Payload::Resident(Arc::from(text))
        } else {
            Payload::paged(build(access, text.as_bytes(), true)?)
        };
        let cached = OnceLock::new();
        if text.len() <= IMMUTABLE_INLINE_MAX as usize {
            let _ = cached.set(name.clone());
        }
        let key = OnceLock::new();
        let _ = key.set(metadata_key(name));
        let value = Self {
            text: TextRef {
                normal: Default::default(),
                summary: text_summary(text),
                body,
            },
            cached: Arc::new(cached),
            key: Arc::new(key),
        };
        access.retain_metadata_name(name, &value);
        Ok(value)
    }
    pub fn resolve(&self, access: &Access<'_>) -> Result<Name, HeapFault> {
        if let Some(name) = self.cached.get() {
            return Ok(name.clone());
        }
        let name = match &self.text.body {
            Payload::Resident(text) => Name::new(text.to_string()),
            Payload::Paged(page) => {
                if let Some(name) = access.projected_name(**page, self.text.summary) {
                    return Ok(name);
                }
                let mut bytes = Vec::new();
                visit(
                    access,
                    **page,
                    self.text.summary,
                    true,
                    0..self.text.summary.utf8_bytes,
                    &mut |chunk| {
                        bytes.extend_from_slice(chunk);
                        ControlFlow::Continue(())
                    },
                )?;
                Name::new(
                    String::from_utf8(bytes).map_err(|e| HeapFault::Malformed(e.to_string()))?,
                )
            }
        };
        if name.as_str().len() <= IMMUTABLE_INLINE_MAX as usize {
            let _ = self.cached.set(name.clone());
        } else if let Payload::Paged(page) = &self.text.body {
            access.retain_projected_name(**page, self.text.summary, &name);
        }
        Ok(name)
    }
    pub fn key(&self, access: &Access<'_>) -> Result<Vec<u8>, HeapFault> {
        if let Some(key) = self.key.get() {
            return Ok(key.clone());
        }
        let key = metadata_key(&self.resolve(access)?);
        let _ = self.key.set(key.clone());
        Ok(key)
    }
    fn matches(&self, name: &Name, access: &Access<'_>) -> Result<bool, HeapFault> {
        if let Some(value) = access.metadata_name(name) {
            return Ok(self.same_version(&value));
        }
        let same = self.resolve(access)? == *name;
        if same {
            access.retain_metadata_name(name, self);
        }
        Ok(same)
    }
    pub fn same_version(&self, other: &Self) -> bool {
        if self.text.summary != other.text.summary {
            return false;
        }
        match (&self.text.body, &other.text.body) {
            (Payload::Paged(a), Payload::Paged(b)) => a.id == b.id,
            (Payload::Resident(a), Payload::Resident(b)) => a == b,
            _ => false,
        }
    }
    pub fn children(&self, out: &mut Vec<PageRef>) {
        if let Payload::Paged(page) = &self.text.body {
            out.push(**page);
        }
    }
    pub fn grants(&mut self, children: &[PageRef]) -> Result<(), HeapFault> {
        if let Payload::Paged(page) = &mut self.text.body {
            grant(Arc::make_mut(page), children)?;
        }
        Ok(())
    }
}
impl TokenCodec for NameRef {
    fn encode(&self, out: &mut TokenWriter) {
        self.text.encode(out);
    }
    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        Ok(Self {
            text: TextRef::decode(input)?,
            cached: Default::default(),
            key: Default::default(),
        })
    }
}
impl Cell for NameRef {
    fn cost(&self) -> u64 {
        0
    }
    fn same_version(&self, other: &Self) -> bool {
        self.same_version(other)
    }
    fn children(&self, out: &mut Vec<PageRef>) {
        self.children(out);
    }
    fn grants(&mut self, children: &[PageRef]) -> Result<(), HeapFault> {
        self.grants(children)
    }
}
impl Cell for (NameRef, EntityId) {
    fn cost(&self) -> u64 {
        8
    }
    fn children(&self, out: &mut Vec<PageRef>) {
        self.0.children(out);
    }
    fn grants(&mut self, children: &[PageRef]) -> Result<(), HeapFault> {
        self.0.grants(children)
    }
}

#[derive(Clone, Debug)]
enum UnitRef {
    Main,
    Function(NameRef),
    Library(FunctionId),
}
impl TokenCodec for UnitRef {
    fn encode(&self, out: &mut TokenWriter) {
        match self {
            Self::Main => out.sum(0, 0),
            Self::Function(n) => {
                out.sum(1, 1);
                n.encode(out);
            }
            Self::Library(id) => {
                out.sum(2, 1);
                id.encode(out);
            }
        }
    }
    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        let (tag, fields) = input.sum()?;
        match tag {
            0 => {
                TokenReader::expect_fields(fields, 0)?;
                Ok(Self::Main)
            }
            1 => {
                TokenReader::expect_fields(fields, 1)?;
                Ok(Self::Function(NameRef::decode(input)?))
            }
            2 => {
                TokenReader::expect_fields(fields, 1)?;
                Ok(Self::Library(FunctionId::decode(input)?))
            }
            _ => Err(TokenError::UnsupportedVariant { variant: tag }),
        }
    }
}

#[derive(Clone)]
pub struct SiteRef {
    unit: UnitRef,
    path: Tree<u32>,
    cached: Arc<OnceLock<Arc<Site>>>,
}
impl std::fmt::Debug for SiteRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SiteRef")
            .field("unit", &self.unit)
            .field("path_len", &self.path.len())
            .finish()
    }
}
impl SiteRef {
    pub fn new(site: &Site, access: &Access<'_>) -> Result<Self, HeapFault> {
        if let Some(value) = access.metadata_site(site) {
            return Ok(value);
        }
        let unit = match &site.unit {
            Unit::Main => UnitRef::Main,
            Unit::Function(name) => UnitRef::Function(NameRef::new(name, access)?),
            Unit::Library(id) => UnitRef::Library(*id),
        };
        let mut path = Tree::new(true);
        for (index, step) in site.path.iter().enumerate() {
            path.write(ordinal_key(index as u64), *step, access)?;
        }
        path.seal_nested(access, &mut budget())?;
        let cached = OnceLock::new();
        let small = encode_body(site).len() <= super::tree::INLINE_TREE_BYTES;
        if small {
            let _ = cached.set(Arc::new(site.clone()));
        }
        let value = Self {
            unit,
            path,
            cached: Arc::new(cached),
        };
        let resident = match &value.unit {
            UnitRef::Function(name) => matches!(name.text.body, Payload::Resident(_)),
            UnitRef::Main | UnitRef::Library(_) => true,
        };
        if small && resident && value.path.is_self_contained() {
            access.retain_metadata_site(site, &value);
        }
        Ok(value)
    }
    pub fn resolve(&self, access: &Access<'_>) -> Result<Arc<Site>, HeapFault> {
        if let Some(site) = self.cached.get() {
            return Ok(Arc::clone(site));
        }
        let unit = match &self.unit {
            UnitRef::Main => Unit::Main,
            UnitRef::Function(name) => Unit::Function(name.resolve(access)?),
            UnitRef::Library(id) => Unit::Library(*id),
        };
        let mut path = Vec::new();
        for index in 0..self.path.len() {
            path.push(
                self.path
                    .rank_get(index, access)?
                    .ok_or_else(|| HeapFault::Malformed("site coordinate is missing".into()))?
                    .1,
            );
        }
        let site = Arc::new(Site::new(unit, path));
        if encode_body(site.as_ref()).len() <= super::tree::INLINE_TREE_BYTES {
            let _ = self.cached.set(Arc::clone(&site));
        }
        Ok(site)
    }
    pub fn matches(&self, site: &Site, access: &Access<'_>) -> Result<bool, HeapFault> {
        if let Some(cached) = self.cached.get() {
            return Ok(cached.as_ref() == site);
        }
        let same_unit = match (&self.unit, &site.unit) {
            (UnitRef::Main, Unit::Main) => true,
            (UnitRef::Library(a), Unit::Library(b)) => a == b,
            (UnitRef::Function(a), Unit::Function(b)) => a.matches(b, access)?,
            _ => false,
        };
        if !same_unit || self.path.len() != site.path.len() as u64 {
            return Ok(false);
        }
        for (index, step) in site.path.iter().enumerate() {
            if self
                .path
                .rank_get(index as u64, access)?
                .map(|(_, step)| step)
                != Some(*step)
            {
                return Ok(false);
            }
        }
        Ok(true)
    }
    pub fn same_version(&self, other: &Self) -> bool {
        let unit_same = match (&self.unit, &other.unit) {
            (UnitRef::Main, UnitRef::Main) => true,
            (UnitRef::Function(a), UnitRef::Function(b)) => a.same_version(b),
            (UnitRef::Library(a), UnitRef::Library(b)) => a == b,
            _ => false,
        };
        unit_same && self.path.same_version(&other.path)
    }
    pub fn children(&self, out: &mut Vec<PageRef>) {
        if let UnitRef::Function(name) = &self.unit {
            name.children(out);
        }
        self.path.children(out);
    }
    pub fn grants(&mut self, children: &[PageRef]) -> Result<(), HeapFault> {
        if let UnitRef::Function(name) = &mut self.unit {
            name.grants(children)?;
        }
        self.path.grants(children)
    }
}
impl TokenCodec for SiteRef {
    fn encode(&self, out: &mut TokenWriter) {
        out.seq(2);
        self.unit.encode(out);
        self.path.encode(out);
    }
    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        input.expect_struct(2)?;
        Ok(Self {
            unit: UnitRef::decode(input)?,
            path: Tree::decode(input)?,
            cached: Default::default(),
        })
    }
}
impl Cell for u32 {
    fn cost(&self) -> u64 {
        0
    }
}

/// Main has no node. Each spawn names one immutable node and its parent.
#[derive(Clone, Copy, Debug)]
pub struct TaskIdentityRef {
    pub root: Option<PageRef>,
}
impl TaskIdentityRef {
    pub const MAIN: Self = Self { root: None };
    pub fn is_main(self) -> bool {
        self.root.is_none()
    }
    pub fn spawn(
        parent: Self,
        site: SiteRef,
        occurrence: u64,
        access: &Access<'_>,
    ) -> Result<Self, HeapFault> {
        let node = SpawnNode {
            parent,
            site,
            occurrence,
        };
        let mut children = Vec::new();
        children.extend(parent.root);
        node.site.children(&mut children);
        Ok(Self {
            root: Some(access.seal(PageKind::KEYED_LEAF, encode_body(&node), children)?),
        })
    }
    pub fn resolve(self, access: &Access<'_>) -> Result<TaskIdentity, HeapFault> {
        let mut steps = Vec::new();
        let mut next = self;
        let mut seen = BTreeSet::new();
        while let Some(page) = next.root {
            if !seen.insert(page.id) {
                return Err(HeapFault::Malformed("cyclic task ancestry".into()));
            }
            let answer = access.read(page)?;
            let invalid = |reason: String| HeapFault::Corrupt {
                page: page.id,
                reason,
            };
            if answer.kind != PageKind::KEYED_LEAF {
                return Err(invalid("task ancestry node kind mismatch".into()));
            }
            let mut input = TokenReader::new(&answer.tokens);
            let mut node = SpawnNode::decode(&mut input).map_err(|e| invalid(e.to_string()))?;
            input.finish().map_err(|e| invalid(e.to_string()))?;
            if let Some(parent) = &mut node.parent.root {
                grant(parent, &answer.children)?;
            }
            node.site.grants(&answer.children)?;
            steps.push((node.site.resolve(access)?.as_ref().clone(), node.occurrence));
            next = node.parent;
        }
        let mut identity = TaskIdentity::Main;
        for (site, occurrence) in steps.into_iter().rev() {
            identity = TaskIdentity::Spawned(SpawnIdentity {
                parent: Arc::new(identity),
                site,
                occurrence,
            });
        }
        Ok(identity)
    }
    pub fn grants(&mut self, children: &[PageRef]) -> Result<(), HeapFault> {
        if let Some(root) = &mut self.root {
            grant(root, children)?;
        }
        Ok(())
    }
}
impl TokenCodec for TaskIdentityRef {
    fn encode(&self, out: &mut TokenWriter) {
        self.root.encode(out);
    }
    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        Ok(Self {
            root: Option::decode(input)?,
        })
    }
}
struct SpawnNode {
    parent: TaskIdentityRef,
    site: SiteRef,
    occurrence: u64,
}
impl TokenCodec for SpawnNode {
    fn encode(&self, out: &mut TokenWriter) {
        out.seq(3);
        self.parent.encode(out);
        self.site.encode(out);
        self.occurrence.encode(out);
    }
    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        input.expect_struct(3)?;
        Ok(Self {
            parent: TaskIdentityRef::decode(input)?,
            site: SiteRef::decode(input)?,
            occurrence: u64::decode(input)?,
        })
    }
}
