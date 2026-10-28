//! Object descriptors packed directly into directory leaves. Identity
//! edges are semantic data, never storage graph child references.
use super::heap_impl::{PagedHeap, census_fault, keyed};
use super::tree::{Access, Cell, SealWork, Tree, grant};
use super::*;
use super::{FamilyRecord, FrozenRef, HeapWriteError, MembershipRun, ObjectKind, SealBudget};
use lash_kernel_doc::LogicalHeldV1;
use lash_kernel_doc::heap::*;
use std::sync::Arc;
use {
    lash_kernel_value::tokens::TokenCodec, lash_kernel_value::tokens::TokenError,
    lash_kernel_value::tokens::TokenReader, lash_kernel_value::tokens::TokenWriter,
};

mod continuation;

#[derive(Clone)]
pub enum Body {
    List {
        rows: Tree<PageValue>,
        next: u64,
    },
    Table {
        kind: ObjectKind,
        lookup: Tree<Ordinal>,
        rows: Tree<Entry>,
        next: u64,
    },
    Closure {
        callable: lash_kernel_value::names::CallableId,
        site: SiteRef,
        captures: Tree<(NameRef, EntityId)>,
    },
    Variable(PageValue),
}
#[derive(Clone)]
pub struct ObjectHead {
    pub body: Body,
    pub family: FamilyId,
}
#[derive(Clone)]
pub struct Entry {
    pub key: PageValue,
    pub value: PageValue,
}
#[derive(Clone)]
pub struct Ordinal(pub u64);
impl ObjectHead {
    pub fn kind(&self) -> ObjectKind {
        match &self.body {
            Body::List { .. } => ObjectKind::List,
            Body::Table { kind, .. } => *kind,
            Body::Closure { .. } => ObjectKind::Closure,
            Body::Variable(_) => ObjectKind::Variable,
        }
    }
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
    pub fn len(&self) -> u64 {
        match &self.body {
            Body::List { rows, .. } => rows.len(),
            Body::Table { rows, .. } => rows.len(),
            _ => 0,
        }
    }
}
pub fn value_cost(value: &PageValue) -> u64 {
    LogicalHeldV1::local(value)
}

impl Cell for PageValue {
    fn cost(&self) -> u64 {
        value_cost(self)
    }
    fn task_edges(&self) -> u64 {
        lash_kernel_doc::heap_values::task_count(self)
    }
    fn decode_cell(
        input: &mut TokenReader<'_>,
        family: Option<FamilyId>,
    ) -> Result<Self, TokenError> {
        lash_kernel_doc::heap_values::decode_value_with_family(input, family)
    }
    fn children(&self, out: &mut Vec<PageRef>) {
        value_children(self, out);
    }
    fn grants(&mut self, children: &[PageRef]) -> Result<(), HeapFault> {
        value_grants(self, children)
    }
}
impl Cell for EntityId {
    fn cost(&self) -> u64 {
        LogicalHeldV1::ENTITY_EDGE
    }
}
impl Cell for Ordinal {
    fn cost(&self) -> u64 {
        0
    }
}
impl TokenCodec for Ordinal {
    fn encode(&self, out: &mut TokenWriter) {
        out.unsigned(self.0);
    }
    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        input.unsigned().map(Self)
    }
}
impl Cell for Entry {
    fn task_edges(&self) -> u64 {
        self.key
            .task_edges()
            .saturating_add(self.value.task_edges())
    }
    fn decode_cell(
        input: &mut TokenReader<'_>,
        family: Option<FamilyId>,
    ) -> Result<Self, TokenError> {
        input.expect_struct(2)?;
        Ok(Self {
            key: <PageValue as Cell>::decode_cell(input, family)?,
            value: <PageValue as Cell>::decode_cell(input, family)?,
        })
    }
    fn cost(&self) -> u64 {
        value_cost(&self.key).saturating_add(value_cost(&self.value))
    }
    fn children(&self, out: &mut Vec<PageRef>) {
        value_children(&self.key, out);
        value_children(&self.value, out);
    }
    fn grants(&mut self, children: &[PageRef]) -> Result<(), HeapFault> {
        value_grants(&mut self.key, children)?;
        value_grants(&mut self.value, children)
    }
}
impl TokenCodec for Entry {
    fn encode(&self, out: &mut TokenWriter) {
        out.seq(2);
        self.key.encode(out);
        self.value.encode(out);
    }
    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        input.expect_struct(2)?;
        Ok(Self {
            key: PageValue::decode(input)?,
            value: PageValue::decode(input)?,
        })
    }
}
impl Cell for ObjectHead {
    fn task_edges(&self) -> u64 {
        match &self.body {
            Body::List { rows, .. } => rows.task_edges(),
            Body::Table { rows, .. } => rows.task_edges(),
            Body::Variable(v) => v.task_edges(),
            Body::Closure { .. } => 0,
        }
    }
    fn cost(&self) -> u64 {
        match &self.body {
            Body::List { rows, .. } => LogicalHeldV1::object(rows.cost()),
            Body::Table {
                kind: ObjectKind::Record,
                rows,
                ..
            } => LogicalHeldV1::record(rows.cost(), rows.len()),
            Body::Table { rows, .. } => LogicalHeldV1::object(rows.cost()),
            Body::Closure { captures, .. } => LogicalHeldV1::object(captures.cost()),
            Body::Variable(value) => LogicalHeldV1::object(value_cost(value)),
        }
    }
    fn prepare(&mut self, access: &Access<'_>, work: &mut SealWork) -> Result<bool, HeapFault> {
        match &mut self.body {
            Body::List { rows, .. } => rows.seal_nested(access, work),
            Body::Table { lookup, rows, .. } => {
                Ok(lookup.seal_nested(access, work)? && rows.seal_nested(access, work)?)
            }
            Body::Closure { captures, .. } => captures.seal_nested(access, work),
            Body::Variable(_) => Ok(true),
        }
    }
    fn children(&self, out: &mut Vec<PageRef>) {
        match &self.body {
            Body::List { rows, .. } => rows.children(out),
            Body::Table { lookup, rows, .. } => {
                lookup.children(out);
                rows.children(out);
            }
            Body::Closure {
                callable: _,
                site,
                captures,
            } => {
                site.children(out);
                captures.children(out);
            }
            Body::Variable(v) => value_children(v, out),
        }
    }
    fn grants(&mut self, children: &[PageRef]) -> Result<(), HeapFault> {
        match &mut self.body {
            Body::List { rows, .. } => rows.grants(children)?,
            Body::Table { lookup, rows, .. } => {
                lookup.grants(children)?;
                rows.grants(children)?;
            }
            Body::Closure {
                callable: _,
                site,
                captures,
            } => {
                site.grants(children)?;
                captures.grants(children)?;
            }
            Body::Variable(v) => value_grants(v, children)?,
        }
        Ok(())
    }
}
#[cfg(test)]
thread_local! {
    static HEAD_ENCODES: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

impl TokenCodec for ObjectHead {
    fn encode(&self, out: &mut TokenWriter) {
        #[cfg(test)]
        HEAD_ENCODES.set(HEAD_ENCODES.get() + 1);
        out.seq(2);
        self.family.encode(out);
        match &self.body {
            Body::List { rows, next } => {
                out.sum(0, 2);
                rows.encode(out);
                next.encode(out);
            }
            Body::Table {
                kind,
                lookup,
                rows,
                next,
            } => {
                out.sum(
                    match kind {
                        ObjectKind::Map => 1,
                        ObjectKind::Set => 2,
                        _ => 3,
                    },
                    3,
                );
                lookup.encode(out);
                rows.encode(out);
                next.encode(out);
            }
            Body::Closure {
                callable,
                site,
                captures,
            } => {
                out.sum(4, 3);
                callable.encode(out);
                site.encode(out);
                captures.encode(out);
            }
            Body::Variable(value) => {
                out.sum(5, 1);
                value.encode(out);
            }
        }
    }
    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        input.expect_struct(2)?;
        let family = FamilyId::decode(input)?;
        let (tag, fields) = input.sum()?;
        Ok(Self {
            family,
            body: match tag {
                0 => {
                    TokenReader::expect_fields(fields, 2)?;
                    Body::List {
                        rows: Tree::decode(input)?,
                        next: u64::decode(input)?,
                    }
                }
                1..=3 => {
                    TokenReader::expect_fields(fields, 3)?;
                    Body::Table {
                        kind: match tag {
                            1 => ObjectKind::Map,
                            2 => ObjectKind::Set,
                            _ => ObjectKind::Record,
                        },
                        lookup: Tree::decode(input)?,
                        rows: Tree::decode(input)?,
                        next: u64::decode(input)?,
                    }
                }
                4 => {
                    TokenReader::expect_fields(fields, 3)?;
                    Body::Closure {
                        callable: lash_kernel_value::names::CallableId::decode(input)?,
                        site: SiteRef::decode(input)?,
                        captures: Tree::decode(input)?,
                    }
                }
                5 => {
                    TokenReader::expect_fields(fields, 1)?;
                    Body::Variable(PageValue::decode(input)?)
                }
                _ => return Err(TokenError::UnsupportedVariant { variant: tag }),
            },
        })
    }
}
pub fn value_children(value: &PageValue, out: &mut Vec<PageRef>) {
    match value {
        PageValue::Text(TextRef {
            body: TextPayload::Paged(p),
            ..
        })
        | PageValue::Bytes(BytesRef {
            body: Payload::Paged(p),
            ..
        }) => out.push(**p),
        PageValue::Int(IntRef::Paged { digits, .. }) => out.push(**digits),
        PageValue::Tuple(t) => match &t.body {
            Payload::Paged(p) => out.push(**p),
            Payload::Resident(v) => {
                for v in v.iter() {
                    value_children(v, out);
                }
            }
        },
        PageValue::Error(e) => {
            value_children(&PageValue::Text(e.kind.clone()), out);
            value_children(&PageValue::Text(e.message.clone()), out);
            match &e.data {
                Payload::Resident(v) => value_children(v, out),
                Payload::Paged(p) => out.push(**p),
            }
        }
        _ => (),
    }
}
pub fn value_grants(value: &mut PageValue, children: &[PageRef]) -> Result<(), HeapFault> {
    fn text_grants(text: &mut TextRef, children: &[PageRef]) -> Result<(), HeapFault> {
        if let TextPayload::Paged(p) = &mut text.body {
            grant(Arc::make_mut(p), children)?;
        }
        Ok(())
    }
    match value {
        PageValue::Text(t) => text_grants(t, children)?,
        PageValue::Bytes(b) => {
            if let Payload::Paged(p) = &mut b.body {
                grant(Arc::make_mut(p), children)?;
            }
        }
        PageValue::Int(IntRef::Paged { digits, .. }) => grant(Arc::make_mut(digits), children)?,
        PageValue::Tuple(t) => match &mut Arc::make_mut(t).body {
            Payload::Paged(p) => grant(Arc::make_mut(p), children)?,
            Payload::Resident(v) => {
                for v in Arc::make_mut(v) {
                    value_grants(v, children)?;
                }
            }
        },
        PageValue::Error(e) => {
            let e = Arc::make_mut(e);
            text_grants(&mut e.kind, children)?;
            text_grants(&mut e.message, children)?;
            match &mut e.data {
                Payload::Resident(v) => value_grants(Arc::make_mut(v), children)?,
                Payload::Paged(p) => grant(Arc::make_mut(p), children)?,
            }
        }
        _ => (),
    }
    Ok(())
}
pub(super) fn entity_bytes(entity: EntityId) -> [u8; 16] {
    let mut key = [0; 16];
    key[..8].copy_from_slice(&entity.family.0.to_be_bytes());
    key[8..].copy_from_slice(&entity.ordinal.to_be_bytes());
    key
}
pub fn entity_key(entity: EntityId) -> super::tree::Key {
    entity_bytes(entity).into()
}
pub fn ordinal_key(ordinal: u64) -> super::tree::Key {
    ordinal.to_be_bytes().into()
}

/// A task's binding/frame trees are the working representation, not an
/// image rebuilt at park. Only changed paths become editable.
pub mod execution {
    pub use super::continuation::{
        PendingRecord, PhaseRecord, ROLE_CHUNK, ROLE_FLAT, RoleChunk, RoleRecord, RolesRecord,
    };
    use super::*;
    use lash_kernel_doc::TaskId;
    use lash_kernel_value::names::Name;

    /// A cursor holding retains identity roots without another PageValue header.
    #[derive(Clone)]
    pub struct IteratedRecord(pub PageValue);
    impl TokenCodec for IteratedRecord {
        fn encode(&self, out: &mut TokenWriter) {
            self.0.encode(out);
        }
        fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
            Ok(Self(PageValue::decode(input)?))
        }
    }
    impl Cell for IteratedRecord {
        fn cost(&self) -> u64 {
            match &self.0 {
                PageValue::Tuple(tuple) => tuple.summary.cost.0,
                _ => 0,
            }
        }
        fn task_edges(&self) -> u64 {
            self.0.task_edges()
        }
        fn decode_cell(
            input: &mut TokenReader<'_>,
            family: Option<FamilyId>,
        ) -> Result<Self, TokenError> {
            Ok(Self(
                lash_kernel_doc::heap_values::decode_value_with_family(input, family)?,
            ))
        }
        fn children(&self, out: &mut Vec<PageRef>) {
            value_children(&self.0, out);
        }
        fn grants(&mut self, children: &[PageRef]) -> Result<(), HeapFault> {
            value_grants(&mut self.0, children)
        }
    }
    #[derive(Clone)]
    pub struct FrameRecord {
        pub capture_names: Vec<lash_kernel_value::names::Name>,
        pub convention: Option<lash_kernel_doc::StartConvention>,
        pub public_result: Option<lash_kernel_value::types::Type>,
        pub link: lash_kernel_value::content_ids::LinkId,
        pub statement: SiteRef,
        pub bindings: Tree<BindingRecord>,
        pub loops: Tree<LoopRecord>,
        pub finally: Tree<FinallyRecord>,
        pub iterated: Tree<IteratedRecord>,
        pub departing: Tree<PageValue>,
        pub arguments: Option<Tree<PageValue>>,
        /// The action nested in its statement the frame stands at, if it
        /// stands at one (`K-EVAL-008`).
        pub pending: Option<PendingRecord>,
    }
    /// Provenance of an occurrence key, retained even after its helper dies.
    #[derive(Clone, Debug, PartialEq, Eq)]
    pub enum OccurrenceOwner {
        Authored,
        Helper(lash_kernel_value::FunctionId),
    }
    #[derive(Clone, Debug, PartialEq, Eq)]
    pub struct OccurrenceRecord {
        pub owner: OccurrenceOwner,
        pub count: u64,
    }
    impl Cell for OccurrenceRecord {
        fn cost(&self) -> u64 {
            LogicalHeldV1::BOOKKEEPING
        }
    }
    impl TokenCodec for OccurrenceRecord {
        fn encode(&self, out: &mut TokenWriter) {
            out.seq(2);
            match self.owner {
                OccurrenceOwner::Authored => None::<lash_kernel_value::FunctionId>.encode(out),
                OccurrenceOwner::Helper(function) => Some(function).encode(out),
            }
            self.count.encode(out);
        }
        fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
            input.expect_struct(2)?;
            let owner = Option::<lash_kernel_value::FunctionId>::decode(input)?
                .map_or(OccurrenceOwner::Authored, OccurrenceOwner::Helper);
            Ok(Self {
                owner,
                count: u64::decode(input)?,
            })
        }
    }
    #[derive(Clone)]
    pub struct TaskRecord {
        pub identity: TaskIdentityRef,
        pub state: TaskStateRecord,
        pub joiners: Tree<JoinerRecord>,
        pub observed: bool,
        pub passed: bool,
        pub occurrences: Tree<OccurrenceRecord>,
        pub frames: Tree<FrameRecord>,
        /// The authored statement a task spawned from library code, or
        /// rooted in it, was spawned under: its spawner's origin at the
        /// spawn, written once. The task's own frames hold no authored
        /// statement to derive its origin from.
        pub spawn_origin: Option<SiteRef>,
    }
    impl FrameRecord {
        pub fn fresh(link: lash_kernel_value::content_ids::LinkId, statement: SiteRef) -> Self {
            Self {
                capture_names: Vec::new(),
                link,
                convention: None,
                public_result: None,
                statement,
                bindings: Tree::new(false),
                loops: Tree::new(true),
                finally: Tree::new(true),
                iterated: Tree::new(true),
                departing: Tree::new(true),
                arguments: None,
                pending: None,
            }
        }
    }
    impl Cell for u64 {
        fn cost(&self) -> u64 {
            LogicalHeldV1::BOOKKEEPING
        }
    }
    impl Cell for TaskId {
        fn cost(&self) -> u64 {
            LogicalHeldV1::BOOKKEEPING
        }
        fn task_edges(&self) -> u64 {
            1
        }
    }
    impl Cell for Name {
        fn cost(&self) -> u64 {
            LogicalHeldV1::BOOKKEEPING
        }
    }
    impl Cell for LoopRecord {
        fn cost(&self) -> u64 {
            LogicalHeldV1::BOOKKEEPING
        }
        fn children(&self, out: &mut Vec<PageRef>) {
            self.site.children(out);
        }
        fn grants(&mut self, children: &[PageRef]) -> Result<(), HeapFault> {
            self.site.grants(children)
        }
    }
    impl Cell for FinallyRecord {
        fn cost(&self) -> u64 {
            LogicalHeldV1::BOOKKEEPING
        }
        fn children(&self, out: &mut Vec<PageRef>) {
            self.site.children(out);
            if let EnteredRecord::ReturnVia(site) = &self.entered {
                site.children(out);
            }
        }
        fn grants(&mut self, children: &[PageRef]) -> Result<(), HeapFault> {
            self.site.grants(children)?;
            if let EnteredRecord::ReturnVia(site) = &mut self.entered {
                site.grants(children)?;
            }
            Ok(())
        }
    }
    impl Cell for JoinerRecord {
        fn cost(&self) -> u64 {
            LogicalHeldV1::BOOKKEEPING
        }
    }
    impl Cell for WaitRecord {
        fn cost(&self) -> u64 {
            LogicalHeldV1::BOOKKEEPING
        }
        fn children(&self, out: &mut Vec<PageRef>) {
            match &self.phase {
                WaitPhase::Issued { request } => out.push(request.root),
                WaitPhase::Committed(
                    CommittedOutcomeRecord::Completed(value)
                    | CommittedOutcomeRecord::Failed(value)
                    | CommittedOutcomeRecord::Rejected(value),
                ) => out.push(value.root),
                _ => {}
            }
        }
        fn grants(&mut self, children: &[PageRef]) -> Result<(), HeapFault> {
            match &mut self.phase {
                WaitPhase::Issued { request } => grant(&mut request.root, children),
                WaitPhase::Committed(
                    CommittedOutcomeRecord::Completed(value)
                    | CommittedOutcomeRecord::Failed(value)
                    | CommittedOutcomeRecord::Rejected(value),
                ) => grant(&mut value.root, children),
                _ => Ok(()),
            }
        }
    }
    impl Cell for ListJoinRecord {
        fn task_edges(&self) -> u64 {
            self.members.task_edges()
        }
        fn cost(&self) -> u64 {
            LogicalHeldV1::BOOKKEEPING
        }
        fn prepare(&mut self, access: &Access<'_>, work: &mut SealWork) -> Result<bool, HeapFault> {
            self.members.seal_nested(access, work)
        }
        fn children(&self, out: &mut Vec<PageRef>) {
            self.members.children(out);
        }
        fn grants(&mut self, children: &[PageRef]) -> Result<(), HeapFault> {
            self.members.grants(children)
        }
    }
    impl Cell for FunctionRecord {
        fn cost(&self) -> u64 {
            value_cost(&self.meta)
        }
        fn children(&self, out: &mut Vec<PageRef>) {
            self.name.children(out);
            value_children(&self.meta, out);
            out.push(self.closure);
            out.extend(self.captures.values().map(|frozen| frozen.root));
            out.extend(
                self.before
                    .iter()
                    .chain(&self.after)
                    .map(|frozen| frozen.root),
            );
        }
        fn grants(&mut self, children: &[PageRef]) -> Result<(), HeapFault> {
            self.name.grants(children)?;
            value_grants(&mut self.meta, children)?;
            grant(&mut self.closure, children)?;
            for frozen in self
                .captures
                .values_mut()
                .chain(self.before.iter_mut())
                .chain(self.after.iter_mut())
            {
                grant(&mut frozen.root, children)?;
            }
            Ok(())
        }
    }
    impl Cell for BindingRecord {
        fn task_edges(&self) -> u64 {
            match &self.value {
                BoundRecord::Value(v) => v.task_edges(),
                _ => 0,
            }
        }
        fn cost(&self) -> u64 {
            match &self.value {
                BoundRecord::Value(v) => value_cost(v),
                BoundRecord::Shared(_) => 0,
            }
        }
        fn children(&self, out: &mut Vec<PageRef>) {
            self.name.children(out);
            self.declared.children(out);
            if let BoundRecord::Value(v) = &self.value {
                value_children(v, out);
            }
        }
        fn grants(&mut self, children: &[PageRef]) -> Result<(), HeapFault> {
            self.name.grants(children)?;
            self.declared.grants(children)?;
            if let BoundRecord::Value(v) = &mut self.value {
                value_grants(v, children)?;
            }
            Ok(())
        }
    }
    impl Cell for FrameRecord {
        fn same_version(&self, other: &Self) -> bool {
            self.capture_names == other.capture_names
                && self.link == other.link
                && self.convention == other.convention
                && self.public_result == other.public_result
                && self.statement.same_version(&other.statement)
                && self.bindings.same_version(&other.bindings)
                && self.loops.same_version(&other.loops)
                && self.finally.same_version(&other.finally)
                && self.iterated.same_version(&other.iterated)
                && self.departing.same_version(&other.departing)
                && match (&self.arguments, &other.arguments) {
                    (None, None) => true,
                    (Some(a), Some(b)) => a.same_version(b),
                    _ => false,
                }
                && match (&self.pending, &other.pending) {
                    (None, None) => true,
                    (Some(a), Some(b)) => a.same_version(b),
                    _ => false,
                }
        }
        fn task_edges(&self) -> u64 {
            self.bindings.task_edges()
                + self.iterated.task_edges()
                + self.departing.task_edges()
                + self.arguments.as_ref().map_or(0, Tree::task_edges)
                + self.pending.as_ref().map_or(0, PendingRecord::task_edges)
        }
        fn cost(&self) -> u64 {
            LogicalHeldV1::frame([
                self.bindings.cost(),
                self.iterated.cost(),
                self.departing.cost(),
                self.arguments.as_ref().map_or(0, Tree::cost),
                self.pending.as_ref().map_or(0, PendingRecord::cost),
            ])
        }
        fn prepare(&mut self, access: &Access<'_>, work: &mut SealWork) -> Result<bool, HeapFault> {
            Ok(self.bindings.seal_nested(access, work)?
                && self.loops.seal_nested(access, work)?
                && self.finally.seal_nested(access, work)?
                && self.iterated.seal_nested(access, work)?
                && self.departing.seal_nested(access, work)?
                && match &mut self.arguments {
                    Some(a) => a.seal_nested(access, work)?,
                    None => true,
                }
                && match &mut self.pending {
                    Some(p) => p.prepare(access, work)?,
                    None => true,
                })
        }
        fn children(&self, out: &mut Vec<PageRef>) {
            self.statement.children(out);
            self.bindings.children(out);
            self.loops.children(out);
            self.finally.children(out);
            self.iterated.children(out);
            self.departing.children(out);
            if let Some(a) = &self.arguments {
                a.children(out);
            }
            if let Some(p) = &self.pending {
                p.children(out);
            }
        }
        fn grants(&mut self, children: &[PageRef]) -> Result<(), HeapFault> {
            self.statement.grants(children)?;
            self.bindings.grants(children)?;
            self.loops.grants(children)?;
            self.finally.grants(children)?;
            self.iterated.grants(children)?;
            self.departing.grants(children)?;
            if let Some(a) = &mut self.arguments {
                a.grants(children)?;
            }
            if let Some(p) = &mut self.pending {
                p.grants(children)?;
            }
            Ok(())
        }
    }
    impl TaskRecord {
        pub fn is_live(&self) -> bool {
            !matches!(self.state, TaskStateRecord::Ended(_))
        }
        pub fn is_outstanding(&self) -> bool {
            !self.identity.is_main()
                && !self.passed
                && match &self.state {
                    TaskStateRecord::Ended(ended) => ended.is_raised() && !self.observed,
                    _ => true,
                }
        }
        /// The value the record holds: an incoming one, or the result an
        /// ended task retains.
        fn held(&self) -> Option<&PageValue> {
            match &self.state {
                TaskStateRecord::Resuming {
                    incoming: IncomingRecord::Value(v) | IncomingRecord::Raise(v),
                    ..
                } => Some(v),
                TaskStateRecord::Ended(ended) => ended.retained(),
                _ => None,
            }
        }
    }
    impl Cell for TaskRecord {
        fn same_version(&self, other: &Self) -> bool {
            self.identity.root.map(|p| p.id) == other.identity.root.map(|p| p.id)
                && self.observed == other.observed
                && self.passed == other.passed
                && lash_kernel_value::tokens::encode_body(&self.state)
                    == lash_kernel_value::tokens::encode_body(&other.state)
                && self.frames.same_version(&other.frames)
                && self.joiners.same_version(&other.joiners)
                && self.occurrences.same_version(&other.occurrences)
                && match (&self.spawn_origin, &other.spawn_origin) {
                    (None, None) => true,
                    (Some(a), Some(b)) => a.same_version(b),
                    _ => false,
                }
        }
        fn live_members(&self) -> u64 {
            u64::from(self.is_outstanding())
        }
        fn task_edges(&self) -> u64 {
            self.frames.task_edges() + self.held().map_or(0, |v| v.task_edges())
        }

        fn cost(&self) -> u64 {
            LogicalHeldV1::task(self.frames.cost(), self.held())
        }
        fn prepare(&mut self, access: &Access<'_>, work: &mut SealWork) -> Result<bool, HeapFault> {
            Ok(self.joiners.seal_nested(access, work)?
                && self.occurrences.seal_nested(access, work)?
                && self.frames.seal_nested(access, work)?)
        }
        fn children(&self, out: &mut Vec<PageRef>) {
            out.extend(self.identity.root);
            self.joiners.children(out);
            self.occurrences.children(out);
            self.frames.children(out);
            if let Some(origin) = &self.spawn_origin {
                origin.children(out);
            }
            if let Some(v) = self.held() {
                value_children(v, out)
            }
        }
        fn grants(&mut self, children: &[PageRef]) -> Result<(), HeapFault> {
            self.identity.grants(children)?;
            self.joiners.grants(children)?;
            self.occurrences.grants(children)?;
            self.frames.grants(children)?;
            if let Some(origin) = &mut self.spawn_origin {
                origin.grants(children)?;
            }
            let held = match &mut self.state {
                TaskStateRecord::Resuming {
                    incoming: IncomingRecord::Value(v) | IncomingRecord::Raise(v),
                    ..
                } => Some(v),
                TaskStateRecord::Ended(ended) => ended.retained_mut(),
                _ => None,
            };
            if let Some(v) = held {
                value_grants(v, children)?
            }
            Ok(())
        }
    }
    impl TokenCodec for FrameRecord {
        fn encode(&self, out: &mut TokenWriter) {
            out.seq(12);
            self.capture_names.encode(out);
            self.link.encode(out);
            self.convention.encode(out);
            self.public_result.encode(out);
            self.statement.encode(out);
            self.bindings.encode(out);
            self.loops.encode(out);
            self.finally.encode(out);
            self.iterated.encode(out);
            self.departing.encode(out);
            self.arguments.encode(out);
            self.pending.encode(out);
        }
        fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
            input.expect_struct(12)?;
            Ok(Self {
                capture_names: Vec::<lash_kernel_value::names::Name>::decode(input)?,
                link: lash_kernel_value::content_ids::LinkId::decode(input)?,
                convention: Option::<lash_kernel_doc::StartConvention>::decode(input)?,
                public_result: Option::<lash_kernel_value::types::Type>::decode(input)?,
                statement: <SiteRef>::decode(input)?,
                bindings: <Tree<BindingRecord>>::decode(input)?,
                loops: <Tree<LoopRecord>>::decode(input)?,
                finally: <Tree<FinallyRecord>>::decode(input)?,
                iterated: <Tree<IteratedRecord>>::decode(input)?,
                departing: <Tree<PageValue>>::decode(input)?,
                arguments: <Option<Tree<PageValue>>>::decode(input)?,
                pending: <Option<PendingRecord>>::decode(input)?,
            })
        }
    }

    impl TokenCodec for TaskRecord {
        fn encode(&self, out: &mut TokenWriter) {
            out.seq(8);
            self.identity.encode(out);
            self.state.encode(out);
            self.joiners.encode(out);
            self.observed.encode(out);
            self.passed.encode(out);
            self.occurrences.encode(out);
            self.frames.encode(out);
            self.spawn_origin.encode(out);
        }
        fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
            input.expect_struct(8)?;
            Ok(Self {
                identity: <TaskIdentityRef>::decode(input)?,
                state: <TaskStateRecord>::decode(input)?,
                joiners: <Tree<JoinerRecord>>::decode(input)?,
                observed: <bool>::decode(input)?,
                passed: <bool>::decode(input)?,
                occurrences: <Tree<OccurrenceRecord>>::decode(input)?,
                frames: <Tree<FrameRecord>>::decode(input)?,
                spawn_origin: <Option<SiteRef>>::decode(input)?,
            })
        }
    }
}

impl Cell for MembershipRun {
    fn live_members(&self) -> u64 {
        self.len
    }
    fn cost(&self) -> u64 {
        0
    }
}
impl PagedHeap {
    pub(super) fn family_member(
        &self,
        family: &FamilyRecord,
        ordinal: u64,
    ) -> Result<bool, HeapFault> {
        let tree = Tree::<MembershipRun>::of_keyed(&family.membership);
        let rank = tree.passed(&ordinal.to_be_bytes(), &self.access)?;
        let Some((_, run)) = rank
            .checked_sub(1)
            .map(|i| tree.rank_get(i, &self.access))
            .transpose()?
            .flatten()
        else {
            return Ok(false);
        };
        Ok(ordinal >= run.start && ordinal - run.start < run.len)
    }
    /// Reserves a fresh occurrence namespace without constructing any body.
    pub fn register_family(&mut self, frozen: FrozenRef) -> Result<FamilyId, HeapWriteError> {
        let family = self.next_family()?;
        if frozen.summary.entities == 0 {
            return Ok(family);
        }
        let mut membership = Tree::new(false);
        if frozen.summary.entities != 0 {
            membership.write(
                ordinal_key(0),
                MembershipRun {
                    start: 0,
                    len: frozen.summary.entities,
                },
                &self.access,
            )?;
        }
        membership.seal(
            &self.access,
            &mut SealWork::new(SealBudget {
                pages: u64::MAX,
                bytes: u64::MAX,
            }),
        )?;
        let record = FamilyRecord {
            family,
            base: frozen,
            extent: frozen.summary.entities,
            membership: keyed(&membership)?,
        };
        if let Some(census) = self.census.as_mut() {
            census
                .added(frozen.summary.entities)
                .map_err(census_fault)?;
            self.accounting = census.certificate();
        }
        if !frozen.summary.data_only {
            self.population.closure_entities = self
                .population
                .closure_entities
                .checked_add(1)
                .ok_or_else(|| HeapFault::Malformed("family population overflow".into()))?;
            self.population.task_edges = self
                .population
                .task_edges
                .checked_add(1)
                .ok_or_else(|| HeapFault::Malformed("family population overflow".into()))?;
        }
        self.families
            .write(ordinal_key(family.0), record, &self.access)?;
        self.header = None;
        Ok(family)
    }
    pub(super) fn sweep_family_member(&mut self, entity: EntityId) -> Result<(), HeapFault> {
        let Some(mut family) = self
            .families
            .get(&entity.family.0.to_be_bytes(), &self.access)?
        else {
            return Ok(());
        };
        if entity.ordinal >= family.extent {
            return Err(HeapFault::Malformed(
                "swept a member beyond its family's extent".into(),
            ));
        }
        let mut runs = Tree::<MembershipRun>::of_keyed(&family.membership);
        let live_before = runs.live_members(&self.access)?;
        let rank = runs.passed(&entity.ordinal.to_be_bytes(), &self.access)?;
        let Some((key, run)) = rank
            .checked_sub(1)
            .map(|i| runs.rank_get(i, &self.access))
            .transpose()?
            .flatten()
        else {
            // Before the first live run: already swept.
            return Ok(());
        };
        let offset = entity
            .ordinal
            .checked_sub(run.start)
            .ok_or_else(|| HeapFault::Malformed("membership run key and start disagree".into()))?;
        if offset >= run.len {
            // In the gap after its run: already swept.
            return Ok(());
        }
        let end = run
            .start
            .checked_add(run.len)
            .filter(|end| *end <= family.extent)
            .ok_or_else(|| HeapFault::Malformed("membership exceeds family extent".into()))?;
        runs.remove(&key, &self.access)?;
        for (start, end) in [(run.start, entity.ordinal), (entity.ordinal + 1, end)] {
            if start < end {
                runs.write(
                    ordinal_key(start),
                    MembershipRun {
                        start,
                        len: end - start,
                    },
                    &self.access,
                )?;
            }
        }
        if runs.live_members(&self.access)?
            != live_before
                .checked_sub(1)
                .ok_or_else(|| HeapFault::Malformed("membership count underflow".into()))?
        {
            return Err(HeapFault::Malformed(
                "membership edit changed the wrong count".into(),
            ));
        }
        if live_before == 1 {
            self.families
                .remove(&entity.family.0.to_be_bytes(), &self.access)?;
            if !family.base.summary.data_only {
                self.population.closure_entities = self
                    .population
                    .closure_entities
                    .checked_sub(1)
                    .ok_or_else(|| HeapFault::Malformed("family population underflow".into()))?;
                self.population.task_edges =
                    self.population.task_edges.checked_sub(1).ok_or_else(|| {
                        HeapFault::Malformed("family task population underflow".into())
                    })?;
            }
            self.header = None;
            return Ok(());
        }
        runs.seal(
            &self.access,
            &mut SealWork::new(SealBudget {
                pages: u64::MAX,
                bytes: u64::MAX,
            }),
        )?;
        family.membership = keyed(&runs)?;
        self.families
            .write(ordinal_key(entity.family.0), family, &self.access)?;
        Ok(())
    }
}

pub(super) fn tuple_tree(tuple: &TupleRef, page: PageRef) -> Tree<PageValue> {
    let tree = Tree::<PageValue>::open(Some(page), tuple.summary.len, tuple.summary.cost, true)
        .with_task_edges(tuple.summary.task_edges);
    match tuple.family {
        Some(family) => tree.in_family(family),
        None => tree,
    }
}

pub(super) fn value_has_edges(value: &PageValue) -> bool {
    matches!(
        value,
        PageValue::List(_)
            | PageValue::Map(_)
            | PageValue::Set(_)
            | PageValue::Record(_)
            | PageValue::Closure(_)
            | PageValue::Task(_)
            | PageValue::Ref(_)
    ) || matches!(value, PageValue::Tuple(t) if t.summary.has_edges)
        || matches!(value, PageValue::Error(e) if e.has_edges)
}
