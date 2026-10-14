//! Object descriptors packed directly into directory leaves. Identity
//! edges are semantic data, never storage graph child references.
use super::heap_impl::{PagedHeap, census_fault, keyed};
use super::tree::{Access, Cell, SealWork, Tree, checked_sum, grant};
use super::*;
use super::{
    FamilyRecord, FrozenRef, HeapWriteError, MembershipRun, ObjectKind, SealBudget, TransientRoots,
};
use lash_kernel_doc::Site;
use lash_kernel_doc::TaskId;
use lash_kernel_doc::graph_tokens::{TokenCodec, TokenError, TokenReader, TokenWriter};
use lash_kernel_doc::heap::*;
use std::ops::ControlFlow;
use std::sync::Arc;

#[derive(Clone)]
pub enum Body {
    List {
        inline: Option<Vec<Value>>,
        rows: Tree<Value>,
        next: u64,
    },
    Table {
        kind: ObjectKind,
        lookup: Tree<Ordinal>,
        rows: Tree<Entry>,
        next: u64,
    },
    Closure {
        site: Site,
        captures: Tree<(lash_kernel_doc::Name, EntityId)>,
    },
    Variable(Value),
}
#[derive(Clone)]
pub struct ObjectHead {
    pub body: Body,
    pub family: FamilyId,
}
#[derive(Clone)]
pub struct Entry {
    pub key: Value,
    pub value: Value,
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
    pub fn len(&self) -> u64 {
        match &self.body {
            Body::List { inline, rows, .. } => inline
                .as_ref()
                .map_or_else(|| rows.len(), |v| v.len() as u64),
            Body::Table { rows, .. } => rows.len(),
            _ => 0,
        }
    }
}
pub fn value_cost(value: &Value) -> u64 {
    lash_kernel_doc::heap_values::local_cost(value)
}

impl Cell for Value {
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
impl Cell for (lash_kernel_doc::Name, EntityId) {
    fn cost(&self) -> u64 {
        8
    }
}
impl Cell for EntityId {
    fn cost(&self) -> u64 {
        8
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
            key: <Value as Cell>::decode_cell(input, family)?,
            value: <Value as Cell>::decode_cell(input, family)?,
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
            key: Value::decode(input)?,
            value: Value::decode(input)?,
        })
    }
}
impl Cell for ObjectHead {
    fn task_edges(&self) -> u64 {
        match &self.body {
            Body::List {
                inline: Some(v), ..
            } => v.iter().map(Cell::task_edges).sum(),
            Body::List { rows, .. } => rows.task_edges(),
            Body::Table { rows, .. } => rows.task_edges(),
            Body::Variable(v) => v.task_edges(),
            Body::Closure { .. } => 0,
        }
    }
    fn cost(&self) -> u64 {
        32u64.saturating_add(match &self.body {
            Body::List { inline, rows, .. } => inline
                .as_ref()
                .map_or_else(|| rows.cost(), |v| v.iter().map(value_cost).sum()),
            Body::Table {
                kind: ObjectKind::Record,
                rows,
                ..
            } => rows.cost().saturating_sub(rows.len() * 16),
            Body::Table { rows, .. } => rows.cost(),
            Body::Closure { captures, .. } => captures.cost(),
            Body::Variable(value) => value_cost(value),
        })
    }
    fn prepare(&mut self, access: &Access<'_>, work: &mut SealWork) -> Result<bool, HeapFault> {
        match &mut self.body {
            Body::List { rows, .. } => rows.seal(access, work),
            Body::Table { lookup, rows, .. } => {
                Ok(lookup.seal(access, work)? && rows.seal(access, work)?)
            }
            Body::Closure { captures, .. } => captures.seal(access, work),
            Body::Variable(_) => Ok(true),
        }
    }
    fn children(&self, out: &mut Vec<PageRef>) {
        match &self.body {
            Body::List { inline, rows, .. } => {
                if let Some(v) = inline {
                    for value in v {
                        value_children(value, out);
                    }
                }
                if let Some(p) = rows.page() {
                    out.push(p);
                }
            }
            Body::Table { lookup, rows, .. } => {
                out.extend(lookup.page());
                out.extend(rows.page());
            }
            Body::Closure { captures, .. } => out.extend(captures.page()),
            Body::Variable(v) => value_children(v, out),
        }
    }
    fn grants(&mut self, children: &[PageRef]) -> Result<(), HeapFault> {
        match &mut self.body {
            Body::List { inline, rows, .. } => {
                if let Some(v) = inline {
                    for value in v {
                        value_grants(value, children)?;
                    }
                }
                rows.grants(children)?;
            }
            Body::Table { lookup, rows, .. } => {
                lookup.grants(children)?;
                rows.grants(children)?;
            }
            Body::Closure { captures, .. } => captures.grants(children)?,
            Body::Variable(v) => value_grants(v, children)?,
        }
        Ok(())
    }
}
impl TokenCodec for ObjectHead {
    fn encode(&self, out: &mut TokenWriter) {
        out.seq(2);
        self.family.encode(out);
        match &self.body {
            Body::List { inline, rows, next } => {
                out.sum(0, 3);
                inline.encode(out);
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
            Body::Closure { site, captures } => {
                out.sum(4, 2);
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
                    TokenReader::expect_fields(fields, 3)?;
                    let inline: Option<Vec<Value>> = Option::decode(input)?;
                    let rows = Tree::decode(input)?;
                    let next = u64::decode(input)?;
                    if inline.is_some() && rows.len() != 0 {
                        return Err(TokenError::Invalid("two list bodies".into()));
                    }
                    Body::List { inline, rows, next }
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
                    TokenReader::expect_fields(fields, 2)?;
                    Body::Closure {
                        site: Site::decode(input)?,
                        captures: Tree::decode(input)?,
                    }
                }
                5 => {
                    TokenReader::expect_fields(fields, 1)?;
                    Body::Variable(Value::decode(input)?)
                }
                _ => return Err(TokenError::UnsupportedVariant { variant: tag }),
            },
        })
    }
}
pub fn value_children(value: &Value, out: &mut Vec<PageRef>) {
    match value {
        Value::Text(TextRef {
            body: Payload::Paged(p),
            ..
        })
        | Value::Bytes(BytesRef {
            body: Payload::Paged(p),
            ..
        }) => out.push(*p),
        Value::Int(IntRef::Paged { digits, .. }) => out.push(*digits),
        Value::Tuple(t) => match &t.body {
            Payload::Paged(p) => out.push(*p),
            Payload::Resident(v) => {
                for v in v.iter() {
                    value_children(v, out);
                }
            }
        },
        Value::Error(e) => {
            value_children(&Value::Text(e.kind.clone()), out);
            value_children(&Value::Text(e.message.clone()), out);
            match &e.data {
                Payload::Resident(v) => value_children(v, out),
                Payload::Paged(p) => out.push(*p),
            }
        }
        _ => (),
    }
}
pub fn value_grants(value: &mut Value, children: &[PageRef]) -> Result<(), HeapFault> {
    fn text_grants(text: &mut TextRef, children: &[PageRef]) -> Result<(), HeapFault> {
        if let Payload::Paged(p) = &mut text.body {
            grant(p, children)?;
        }
        Ok(())
    }
    match value {
        Value::Text(t) => text_grants(t, children)?,
        Value::Bytes(b) => {
            if let Payload::Paged(p) = &mut b.body {
                grant(p, children)?;
            }
        }
        Value::Int(IntRef::Paged { digits, .. }) => grant(digits, children)?,
        Value::Tuple(t) => match &mut t.body {
            Payload::Paged(p) => grant(p, children)?,
            Payload::Resident(v) => {
                for v in Arc::make_mut(v) {
                    value_grants(v, children)?;
                }
            }
        },
        Value::Error(e) => {
            let e = Arc::make_mut(e);
            text_grants(&mut e.kind, children)?;
            text_grants(&mut e.message, children)?;
            match &mut e.data {
                Payload::Resident(v) => value_grants(Arc::make_mut(v), children)?,
                Payload::Paged(p) => grant(p, children)?,
            }
        }
        _ => (),
    }
    Ok(())
}
pub fn entity_key(entity: EntityId) -> Vec<u8> {
    [entity.family.0.to_be_bytes(), entity.ordinal.to_be_bytes()].concat()
}
pub fn ordinal_key(ordinal: u64) -> Vec<u8> {
    ordinal.to_be_bytes().to_vec()
}

/// A task's binding/frame trees are the working representation, not an
/// image rebuilt at park. Only changed paths become editable.
pub mod execution {
    use super::*;
    use lash_kernel_doc::{Name, TaskId, TaskIdentity};

    /// A cursor holding retains identity roots without another Value header.
    #[derive(Clone)]
    pub struct IteratedRecord(pub Value);
    impl TokenCodec for IteratedRecord {
        fn encode(&self, out: &mut TokenWriter) {
            self.0.encode(out);
        }
        fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
            Ok(Self(Value::decode(input)?))
        }
    }
    impl Cell for IteratedRecord {
        fn cost(&self) -> u64 {
            match &self.0 {
                Value::Tuple(tuple) => tuple.summary.cost.0,
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
        pub statement: Site,
        pub bindings: Tree<BindingRecord>,
        pub loops: Tree<LoopRecord>,
        pub finally: Tree<FinallyRecord>,
        pub iterated: Tree<IteratedRecord>,
        pub departing: Tree<Value>,
        pub arguments: Option<Tree<Value>>,
    }
    #[derive(Clone)]
    pub struct TaskRecord {
        pub identity: TaskIdentity,
        pub state: TaskStateRecord,
        pub result_retained: bool,
        pub joiners: Tree<JoinerRecord>,
        pub observed: bool,
        pub passed: bool,
        pub failed: bool,
        pub occurrences: Tree<u64>,
        pub frames: Tree<FrameRecord>,
    }
    impl FrameRecord {
        pub fn fresh(statement: Site) -> Self {
            Self {
                statement,
                bindings: Tree::new(false),
                loops: Tree::new(true),
                finally: Tree::new(true),
                iterated: Tree::new(true),
                departing: Tree::new(true),
                arguments: None,
            }
        }
    }
    impl Cell for u64 {
        fn cost(&self) -> u64 {
            0
        }
    }
    impl Cell for TaskId {
        fn cost(&self) -> u64 {
            0
        }
        fn task_edges(&self) -> u64 {
            1
        }
    }
    impl Cell for Name {
        fn cost(&self) -> u64 {
            0
        }
    }
    impl Cell for LoopRecord {
        fn cost(&self) -> u64 {
            0
        }
    }
    impl Cell for FinallyRecord {
        fn cost(&self) -> u64 {
            0
        }
    }
    impl Cell for JoinerRecord {
        fn cost(&self) -> u64 {
            0
        }
    }
    impl Cell for WaitRecord {
        fn cost(&self) -> u64 {
            0
        }
        fn children(&self, out: &mut Vec<PageRef>) {
            match &self.phase {
                WaitPhase::Issued { request } => out.push(request.root),
                WaitPhase::Committed(
                    CommittedOutcomeRecord::Completed(value)
                    | CommittedOutcomeRecord::Failed(value),
                ) => out.push(value.root),
                _ => {}
            }
        }
        fn grants(&mut self, children: &[PageRef]) -> Result<(), HeapFault> {
            match &mut self.phase {
                WaitPhase::Issued { request } => grant(&mut request.root, children),
                WaitPhase::Committed(
                    CommittedOutcomeRecord::Completed(value)
                    | CommittedOutcomeRecord::Failed(value),
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
            0
        }
        fn prepare(&mut self, access: &Access<'_>, work: &mut SealWork) -> Result<bool, HeapFault> {
            self.members.seal(access, work)
        }
        fn children(&self, out: &mut Vec<PageRef>) {
            out.extend(self.members.page());
        }
        fn grants(&mut self, children: &[PageRef]) -> Result<(), HeapFault> {
            self.members.grants(children)
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
            if let BoundRecord::Value(v) = &self.value {
                value_children(v, out);
            }
        }
        fn grants(&mut self, children: &[PageRef]) -> Result<(), HeapFault> {
            if let BoundRecord::Value(v) = &mut self.value {
                value_grants(v, children)?;
            }
            Ok(())
        }
    }
    impl Cell for FrameRecord {
        fn task_edges(&self) -> u64 {
            self.bindings.task_edges()
                + self.iterated.task_edges()
                + self.departing.task_edges()
                + self.arguments.as_ref().map_or(0, Tree::task_edges)
        }
        fn cost(&self) -> u64 {
            64 + self.bindings.cost()
                + self.iterated.cost()
                + self.departing.cost()
                + self.arguments.as_ref().map_or(0, Tree::cost)
        }
        fn prepare(&mut self, access: &Access<'_>, work: &mut SealWork) -> Result<bool, HeapFault> {
            Ok(self.bindings.seal(access, work)?
                && self.loops.seal(access, work)?
                && self.finally.seal(access, work)?
                && self.iterated.seal(access, work)?
                && self.departing.seal(access, work)?
                && match &mut self.arguments {
                    Some(a) => a.seal(access, work)?,
                    None => true,
                })
        }
        fn children(&self, out: &mut Vec<PageRef>) {
            out.extend(self.bindings.page());
            out.extend(self.loops.page());
            out.extend(self.finally.page());
            out.extend(self.iterated.page());
            out.extend(self.departing.page());
            if let Some(a) = &self.arguments {
                out.extend(a.page());
            }
        }
        fn grants(&mut self, children: &[PageRef]) -> Result<(), HeapFault> {
            self.bindings.grants(children)?;
            self.loops.grants(children)?;
            self.finally.grants(children)?;
            self.iterated.grants(children)?;
            self.departing.grants(children)?;
            if let Some(a) = &mut self.arguments {
                a.grants(children)?;
            }
            Ok(())
        }
    }
    impl Cell for TaskRecord {
        fn task_edges(&self) -> u64 {
            self.frames.task_edges()
                + match &self.state {
                    TaskStateRecord::Resuming(
                        IncomingRecord::Value(v) | IncomingRecord::Raise(v),
                    )
                    | TaskStateRecord::Ended(EndedTask::Returned(v) | EndedTask::Raised(v)) => {
                        v.task_edges()
                    }
                    _ => 0,
                }
        }

        fn cost(&self) -> u64 {
            self.frames.cost()
                + match &self.state {
                    TaskStateRecord::Resuming(
                        IncomingRecord::Value(v) | IncomingRecord::Raise(v),
                    ) => value_cost(v),
                    TaskStateRecord::Ended(EndedTask::Returned(v) | EndedTask::Raised(v))
                        if self.result_retained =>
                    {
                        value_cost(v)
                    }
                    _ => 0,
                }
        }
        fn prepare(&mut self, access: &Access<'_>, work: &mut SealWork) -> Result<bool, HeapFault> {
            Ok(self.joiners.seal(access, work)?
                && self.occurrences.seal(access, work)?
                && self.frames.seal(access, work)?)
        }
        fn children(&self, out: &mut Vec<PageRef>) {
            out.extend(self.joiners.page());
            out.extend(self.occurrences.page());
            out.extend(self.frames.page());
            match &self.state {
                TaskStateRecord::Resuming(IncomingRecord::Value(v) | IncomingRecord::Raise(v))
                | TaskStateRecord::Ended(EndedTask::Returned(v) | EndedTask::Raised(v)) => {
                    value_children(v, out)
                }
                _ => (),
            }
        }
        fn grants(&mut self, children: &[PageRef]) -> Result<(), HeapFault> {
            self.joiners.grants(children)?;
            self.occurrences.grants(children)?;
            self.frames.grants(children)?;
            match &mut self.state {
                TaskStateRecord::Resuming(IncomingRecord::Value(v) | IncomingRecord::Raise(v))
                | TaskStateRecord::Ended(EndedTask::Returned(v) | EndedTask::Raised(v)) => {
                    value_grants(v, children)?
                }
                _ => (),
            }
            Ok(())
        }
    }
    impl TokenCodec for FrameRecord {
        fn encode(&self, out: &mut TokenWriter) {
            out.seq(7);
            self.statement.encode(out);
            self.bindings.encode(out);
            self.loops.encode(out);
            self.finally.encode(out);
            self.iterated.encode(out);
            self.departing.encode(out);
            self.arguments.encode(out);
        }
        fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
            input.expect_struct(7)?;
            Ok(Self {
                statement: <Site>::decode(input)?,
                bindings: <Tree<BindingRecord>>::decode(input)?,
                loops: <Tree<LoopRecord>>::decode(input)?,
                finally: <Tree<FinallyRecord>>::decode(input)?,
                iterated: <Tree<IteratedRecord>>::decode(input)?,
                departing: <Tree<Value>>::decode(input)?,
                arguments: <Option<Tree<Value>>>::decode(input)?,
            })
        }
    }

    impl TokenCodec for TaskRecord {
        fn encode(&self, out: &mut TokenWriter) {
            out.seq(9);
            self.identity.encode(out);
            self.state.encode(out);
            self.result_retained.encode(out);
            self.joiners.encode(out);
            self.observed.encode(out);
            self.passed.encode(out);
            self.failed.encode(out);
            self.occurrences.encode(out);
            self.frames.encode(out);
        }
        fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
            input.expect_struct(9)?;
            Ok(Self {
                identity: <TaskIdentity>::decode(input)?,
                state: <TaskStateRecord>::decode(input)?,
                result_retained: bool::decode(input)?,
                joiners: <Tree<JoinerRecord>>::decode(input)?,
                observed: <bool>::decode(input)?,
                passed: <bool>::decode(input)?,
                failed: <bool>::decode(input)?,
                occurrences: <Tree<u64>>::decode(input)?,
                frames: <Tree<FrameRecord>>::decode(input)?,
            })
        }
    }
}

impl Cell for MembershipRun {
    fn live_members(&self) -> u64 {
        if self.live { self.len } else { 0 }
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
        let tree = Tree::<MembershipRun>::open(
            family.membership.page,
            family.membership.len,
            family.membership.cost,
            false,
        );
        let rank = tree.passed(&ordinal_key(ordinal), &self.access)?;
        let Some((_, run)) = rank
            .checked_sub(1)
            .map(|i| tree.rank_get(i, &self.access))
            .transpose()?
            .flatten()
        else {
            return Ok(false);
        };
        Ok(run.live && ordinal >= run.start && ordinal - run.start < run.len)
    }
    /// Reserves a fresh occurrence namespace without constructing any body.
    pub fn register_family(&mut self, frozen: FrozenRef) -> Result<FamilyId, HeapWriteError> {
        let family = self.next_family()?;
        if frozen.summary.entities == 0 {
            return Ok(family);
        }
        let credit = self.outstanding_reserved().min(frozen.summary.cost.0);
        self.admit(frozen.summary.cost.0 - credit, &[])?;
        self.reserved -= credit;
        let mut membership = Tree::new(false);
        if frozen.summary.entities != 0 {
            membership.write(
                ordinal_key(0),
                MembershipRun {
                    start: 0,
                    len: frozen.summary.entities,
                    live: true,
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
            membership: keyed(&membership),
        };
        let census = self
            .census
            .as_mut()
            .ok_or_else(|| HeapFault::Malformed("family has no census".into()))?;
        census
            .changed(LogicalBytes(credit), LogicalBytes(0))
            .map_err(census_fault)?;
        census
            .added(frozen.summary.cost, frozen.summary.entities)
            .map_err(census_fault)?;
        self.accounting = census.certificate();
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
            .get(&ordinal_key(entity.family.0), &self.access)?
        else {
            return Ok(());
        };
        let mut runs = Tree::<MembershipRun>::open(
            family.membership.page,
            family.membership.len,
            family.membership.cost,
            false,
        );
        let live_before = runs.live_members(&self.access)?;
        let rank = runs.passed(&ordinal_key(entity.ordinal), &self.access)?;
        let Some((key, run)) = rank
            .checked_sub(1)
            .map(|i| runs.rank_get(i, &self.access))
            .transpose()?
            .flatten()
        else {
            return Err(HeapFault::Malformed("family membership has a hole".into()));
        };
        if entity.ordinal < run.start || entity.ordinal - run.start >= run.len {
            return Err(HeapFault::Malformed("family membership has a hole".into()));
        }
        if !run.live {
            return Ok(());
        }
        runs.remove(&key, &self.access)?;
        if entity.ordinal > run.start {
            runs.write(
                ordinal_key(run.start),
                MembershipRun {
                    start: run.start,
                    len: entity.ordinal - run.start,
                    live: true,
                },
                &self.access,
            )?;
        }
        // Merge adjacent tombstones instead of retaining one row per death.
        let mut start = entity.ordinal;
        let mut len = 1;
        if let Some((key, previous)) = runs.rank_get(
            runs.passed(&ordinal_key(start), &self.access)?
                .saturating_sub(1),
            &self.access,
        )? {
            if !previous.live && previous.start + previous.len == start {
                start = previous.start;
                len += previous.len;
                runs.remove(&key, &self.access)?;
            }
        }
        let tail = entity.ordinal + 1;
        let end = run
            .start
            .checked_add(run.len)
            .ok_or_else(|| HeapFault::Malformed("family extent overflow".into()))?;
        if tail < end {
            runs.write(
                ordinal_key(tail),
                MembershipRun {
                    start: tail,
                    len: end - tail,
                    live: true,
                },
                &self.access,
            )?;
        } else if let Some((key, next)) =
            runs.after(Some(&ordinal_key(entity.ordinal)), &self.access)?
        {
            if !next.live && next.start == tail {
                len += next.len;
                runs.remove(&key, &self.access)?;
            }
        }
        runs.write(
            ordinal_key(start),
            MembershipRun {
                start,
                len,
                live: false,
            },
            &self.access,
        )?;
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
                .remove(&ordinal_key(entity.family.0), &self.access)?;
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
        family.membership = keyed(&runs);
        self.families
            .write(ordinal_key(entity.family.0), family, &self.access)?;
        Ok(())
    }
}

impl PagedHeap {
    /// Installs an authenticated binding tree in place. A precision check sees
    /// the proposed roots, so dropped bindings cannot cause a false refusal.
    pub fn set_session_root(
        &mut self,
        bindings: Tree<BindingRecord>,
    ) -> Result<(), HeapWriteError> {
        let before = self.session.cost();
        let old_tasks = self.session.read_task_edges(&self.access)?;
        self.replace_execution_root(before, old_tasks, bindings, |heap, root| {
            heap.session = root
        })
    }
    pub fn set_task_root(&mut self, tasks: Tree<TaskRecord>) -> Result<(), HeapWriteError> {
        let before = self.tasks.cost();
        let old_tasks = self.tasks.read_task_edges(&self.access)?;
        self.replace_execution_root(before, old_tasks, tasks, |heap, root| heap.tasks = root)
    }
    pub fn set_join_root(&mut self, joins: Tree<ListJoinRecord>) -> Result<(), HeapWriteError> {
        let before = self.joins.cost();
        let old_tasks = self.joins.read_task_edges(&self.access)?;
        self.replace_execution_root(before, old_tasks, joins, |heap, root| heap.joins = root)
    }
    fn replace_execution_root<T: Cell>(
        &mut self,
        before: u64,
        old_tasks: u64,
        root: Tree<T>,
        install: impl FnOnce(&mut Self, Tree<T>),
    ) -> Result<(), HeapWriteError> {
        let after = root.cost();
        let new_tasks = root.read_task_edges(&self.access)?;
        let root = root.with_task_edges(new_tasks);
        let mut census = self
            .census
            .take()
            .ok_or_else(|| HeapFault::Malformed("persistent root has no census".into()))?;
        let credit = self.reserved.min(after.saturating_sub(before));
        let decision = census.reserve(LogicalBytes(
            after.saturating_sub(before).saturating_sub(credit),
        ));
        let result = (|| {
            census
                .changed(LogicalBytes(credit), LogicalBytes(0))
                .map_err(census_fault)?;
            self.reserved -= credit;
            install(self, root);
            self.population.task_edges = self
                .population
                .task_edges
                .checked_sub(old_tasks)
                .and_then(|v| v.checked_add(new_tasks))
                .ok_or_else(|| HeapFault::Malformed("root task-edge population overflow".into()))?;
            self.header = None;
            match decision {
                ReservationDecision::Granted(_) => census
                    .changed(LogicalBytes(before), LogicalBytes(after))
                    .map_err(census_fault)
                    .map_err(HeapWriteError::from),
                ReservationDecision::NeedsPrecision(_) => {
                    let registers = self.statement_roots().to_vec();
                    let reserved = self.outstanding_reserved();
                    match census.refine(
                        self,
                        &TransientRoots {
                            registers: &registers,
                            pinned: &[],
                            native_args: &[],
                            fresh: &[],
                            reserved: LogicalBytes(reserved),
                        },
                        PendingReservation {
                            bytes: LogicalBytes(0),
                            overflowed: false,
                        },
                    ) {
                        Ok(RefinedDecision::Granted(_)) => Ok(()),
                        Ok(RefinedDecision::Refused { .. }) => Err(HeapWriteError::Memory),
                        Err(error) => Err(census_fault(error).into()),
                    }
                }
            }
        })();
        self.accounting = census.certificate();
        self.census = Some(census);
        result
    }
}

impl CensusGraph for PagedHeap {
    fn root_cost(&mut self) -> Result<LogicalBytes, HeapFault> {
        // U prices every retained result. Exact L instead prices only results
        // reached through handles, in task_result, so remove them here.
        let mut retained = 0;
        visit_tree(&self.tasks, &self.access, &mut |task| {
            if task.result_retained
                && let TaskStateRecord::Ended(EndedTask::Returned(value) | EndedTask::Raised(value)) =
                    &task.state
            {
                retained = checked_sum([retained, value_cost(value)].into_iter())?;
            }
            Ok(())
        })?;
        let tasks = self.tasks.cost().checked_sub(retained).ok_or_else(|| {
            HeapFault::Malformed("retained task results exceed the task root cost".into())
        })?;
        Ok(LogicalBytes(checked_sum(
            [self.session.cost(), tasks, self.joins.cost()].into_iter(),
        )?))
    }
    fn value_cost(&mut self, value: &Value) -> Result<LogicalBytes, HeapFault> {
        Ok(LogicalBytes(value_cost(value)))
    }
    fn value_edges(
        &mut self,
        value: &Value,
        out: &mut dyn FnMut(Identity) -> Result<(), HeapFault>,
    ) -> Result<(), HeapFault> {
        self.stream_edges(value, out)
    }
    fn persistent_roots(
        &mut self,
        visit: &mut dyn FnMut(&Value) -> Result<(), HeapFault>,
    ) -> Result<(), HeapFault> {
        let binding =
            |binding: &BindingRecord, visit: &mut dyn FnMut(&Value) -> Result<(), HeapFault>| {
                match &binding.value {
                    BoundRecord::Value(v) => visit(v),
                    BoundRecord::Shared(id) => visit(&Value::Ref(Identity::Object(*id))),
                }
            };
        visit_tree(&self.session, &self.access, &mut |b| binding(b, visit))?;
        visit_tree(&self.joins, &self.access, &mut |join| {
            visit_tree(&join.members, &self.access, &mut |member| {
                visit(&Value::Task(*member))
            })
        })?;
        visit_tree(&self.tasks, &self.access, &mut |task| {
            match &task.state {
                TaskStateRecord::Resuming(IncomingRecord::Value(v) | IncomingRecord::Raise(v)) => {
                    visit(v)?
                }
                TaskStateRecord::Resuming(IncomingRecord::Joined(t))
                | TaskStateRecord::Joining(t) => visit(&Value::Task(*t))?,
                _ => (),
            }
            visit_tree(&task.frames, &self.access, &mut |frame| {
                visit_tree(&frame.bindings, &self.access, &mut |b| binding(b, visit))?;
                visit_tree(&frame.iterated, &self.access, &mut |cursor| {
                    visit(&cursor.0)
                })?;
                visit_tree(&frame.departing, &self.access, visit)?;
                if let Some(args) = &frame.arguments {
                    visit_tree(args, &self.access, visit)?;
                }
                Ok(())
            })
        })
    }
    fn entity_cost(&mut self, entity: EntityId) -> Result<Option<EntityCost>, HeapFault> {
        Ok(self.head(entity)?.map(|head| EntityCost {
            local: LogicalBytes(head.cost()),
            has_edges: true,
        }))
    }
    fn entity_edges(
        &mut self,
        entity: EntityId,
        out: &mut dyn FnMut(Identity) -> Result<(), HeapFault>,
    ) -> Result<(), HeapFault> {
        let Some(head) = self.head(entity)? else {
            return Ok(());
        };
        let mut error = None;
        match head.body {
            Body::Variable(v) => return self.stream_edges(&v, out),
            Body::Closure { captures, .. } => captures.visit(
                &self.access,
                &mut |_, id| match out(Identity::Object(id.1)) {
                    Ok(()) => true,
                    Err(e) => {
                        error = Some(e);
                        false
                    }
                },
            )?,
            Body::List {
                inline: Some(v), ..
            } => {
                for v in &v {
                    self.stream_edges(v, out)?;
                }
            }
            Body::List { rows, .. } => {
                rows.visit(&self.access, &mut |_, v| match self.stream_edges(v, out) {
                    Ok(()) => true,
                    Err(e) => {
                        error = Some(e);
                        false
                    }
                })?
            }
            Body::Table { rows, .. } => rows.visit(&self.access, &mut |_, e| match self
                .stream_edges(&e.key, out)
                .and_then(|()| self.stream_edges(&e.value, out))
            {
                Ok(()) => true,
                Err(e) => {
                    error = Some(e);
                    false
                }
            })?,
        }
        error.map_or(Ok(()), Err)
    }
    fn task_result(
        &mut self,
        task: TaskId,
        out: &mut dyn FnMut(Identity) -> Result<(), HeapFault>,
    ) -> Result<LogicalBytes, HeapFault> {
        let Some(task) = self.tasks.get(&ordinal_key(task.0), &self.access)? else {
            return Ok(LogicalBytes(0));
        };
        let value = match &task.state {
            TaskStateRecord::Ended(EndedTask::Returned(v) | EndedTask::Raised(v))
                if task.result_retained =>
            {
                v
            }
            _ => return Ok(LogicalBytes(0)),
        };
        self.stream_edges(value, out)?;
        Ok(LogicalBytes(value_cost(value)))
    }
    fn sweep(&mut self, unreachable: &SweepSet) -> Result<(), HeapFault> {
        for entity in &unreachable.entities {
            if entity.family != FamilyId::PRIMARY {
                self.sweep_family_member(*entity)?;
            }
            if let Some(head) = self.directory.remove(&entity_key(*entity), &self.access)? {
                self.population.closure_entities = self
                    .population
                    .closure_entities
                    .checked_sub((head.kind() == ObjectKind::Closure) as u64)
                    .ok_or_else(|| HeapFault::Malformed("sweep population underflow".into()))?;
                self.population.task_edges = self
                    .population
                    .task_edges
                    .checked_sub(head.task_edges())
                    .ok_or_else(|| HeapFault::Malformed("sweep task-edge underflow".into()))?;
            }
        }
        for task in &unreachable.ended_task_results {
            if let Some(mut row) = self.tasks.get(&ordinal_key(task.0), &self.access)? {
                let old_edges = row.task_edges();
                row.state = match row.state {
                    TaskStateRecord::Ended(EndedTask::Returned(_)) => {
                        TaskStateRecord::Ended(EndedTask::Returned(Value::Null))
                    }
                    TaskStateRecord::Ended(EndedTask::Raised(_)) => {
                        TaskStateRecord::Ended(EndedTask::Raised(Value::Null))
                    }
                    _ => {
                        return Err(HeapFault::Malformed(
                            "sweep named a running task result".into(),
                        ));
                    }
                };
                row.result_retained = false;
                self.population.task_edges = self
                    .population
                    .task_edges
                    .checked_sub(old_edges)
                    .and_then(|value| value.checked_add(row.task_edges()))
                    .ok_or_else(|| {
                        HeapFault::Malformed("retired task population underflow".into())
                    })?;
                self.tasks.write(ordinal_key(task.0), row, &self.access)?;
            }
        }
        self.header = None;
        Ok(())
    }
    fn census_after(
        &mut self,
        after: Option<EntityId>,
        limit: usize,
    ) -> Result<Vec<EntityId>, HeapFault> {
        let mut entities = Vec::new();
        if after.is_none_or(|id| id.family == FamilyId::PRIMARY) {
            let mut key = after.map(entity_key);
            while entities.len() < limit {
                let Some((next, head)) = self.directory.after(key.as_deref(), &self.access)? else {
                    break;
                };
                if head.family != FamilyId::PRIMARY {
                    break;
                }
                let ordinal = u64::from_be_bytes(
                    next.get(8..)
                        .ok_or_else(|| HeapFault::Malformed("identity key malformed".into()))?
                        .try_into()
                        .map_err(|_| HeapFault::Malformed("identity key malformed".into()))?,
                );
                entities.push(EntityId {
                    family: FamilyId::PRIMARY,
                    ordinal,
                });
                key = Some(next);
            }
        }
        let mut key = after
            .and_then(|id| id.family.0.checked_sub(1))
            .map(ordinal_key);
        while entities.len() < limit {
            let Some((next, family)) = self.families.after(key.as_deref(), &self.access)? else {
                break;
            };
            let runs = Tree::<MembershipRun>::open(
                family.membership.page,
                family.membership.len,
                family.membership.cost,
                false,
            );
            let first = after
                .filter(|id| id.family == family.family)
                .and_then(|id| id.ordinal.checked_add(1))
                .unwrap_or(0);
            let mut rank = runs
                .passed(&ordinal_key(first), &self.access)?
                .saturating_sub(1);
            while entities.len() < limit {
                let Some((_, run)) = runs.rank_get(rank, &self.access)? else {
                    break;
                };
                let end = run
                    .start
                    .checked_add(run.len)
                    .ok_or_else(|| HeapFault::Malformed("family run extent overflow".into()))?;
                if end > family.extent {
                    return Err(HeapFault::Corrupt {
                        page: family.base.root.id,
                        reason: "membership exceeds family extent".into(),
                    });
                }
                if run.live {
                    let count = (end.saturating_sub(run.start.max(first)))
                        .min((limit - entities.len()) as u64);
                    for ordinal in run.start.max(first)..run.start.max(first) + count {
                        entities.push(EntityId {
                            family: family.family,
                            ordinal,
                        });
                    }
                }
                rank += 1;
            }
            key = Some(next);
        }
        Ok(entities)
    }
    fn ended_tasks_after(
        &mut self,
        after: Option<TaskId>,
        limit: usize,
    ) -> Result<Vec<TaskId>, HeapFault> {
        let mut key = after.map(|t| ordinal_key(t.0));
        let mut tasks = Vec::new();
        while tasks.len() < limit {
            let Some((next, row)) = self.tasks.after(key.as_deref(), &self.access)? else {
                break;
            };
            if !matches!(row.state, TaskStateRecord::Ended(EndedTask::Returned(_) | EndedTask::Raised(_)) if row.result_retained)
            {
                key = Some(next);
                continue;
            }
            tasks.push(TaskId(u64::from_be_bytes(
                next.as_slice()
                    .try_into()
                    .map_err(|_| HeapFault::Malformed("task identity key is not 8 bytes".into()))?,
            )));
            key = Some(next);
        }
        Ok(tasks)
    }
}
impl PagedHeap {
    /// Streams nested immutable edges in place, stopping on the first visitor
    /// failure. Scalar payloads are never loaded by a trace.
    pub fn stream_edges(
        &self,
        value: &Value,
        out: &mut dyn FnMut(Identity) -> Result<(), HeapFault>,
    ) -> Result<(), HeapFault> {
        match value {
            Value::List(id)
            | Value::Map(id)
            | Value::Set(id)
            | Value::Record(id)
            | Value::Closure(id) => out(Identity::Object(*id))?,
            Value::Task(id) => out(Identity::Task(*id))?,
            Value::Ref(id) => out(*id)?,
            Value::Tuple(t) if t.summary.has_edges => {
                let mut error = None;
                self.tuple_visit(t, &mut |v| match self.stream_edges(v, out) {
                    Ok(()) => ControlFlow::Continue(()),
                    Err(e) => {
                        error = Some(e);
                        ControlFlow::Break(())
                    }
                })?;
                if let Some(e) = error {
                    return Err(e);
                }
            }
            Value::Error(e) if e.has_edges => self.stream_edges(&self.error_data(e)?, out)?,
            _ => (),
        }
        Ok(())
    }
}

pub(super) fn tuple_tree(tuple: &TupleRef, page: PageRef) -> Tree<Value> {
    let tree = Tree::<Value>::open(Some(page), tuple.summary.len, tuple.summary.cost, true)
        .with_task_edges(tuple.summary.task_edges);
    match tuple.family {
        Some(family) => tree.in_family(family),
        None => tree,
    }
}

fn visit_tree<T: Cell>(
    tree: &Tree<T>,
    access: &Access<'_>,
    visit: &mut dyn FnMut(&T) -> Result<(), HeapFault>,
) -> Result<(), HeapFault> {
    let mut error = None;
    tree.visit(access, &mut |_, v| match visit(v) {
        Ok(()) => true,
        Err(e) => {
            error = Some(e);
            false
        }
    })?;
    error.map_or(Ok(()), Err)
}

pub(super) fn value_has_edges(value: &Value) -> bool {
    matches!(
        value,
        Value::List(_)
            | Value::Map(_)
            | Value::Set(_)
            | Value::Record(_)
            | Value::Closure(_)
            | Value::Task(_)
            | Value::Ref(_)
    ) || matches!(value, Value::Tuple(t) if t.summary.has_edges)
        || matches!(value, Value::Error(e) if e.has_edges)
}
