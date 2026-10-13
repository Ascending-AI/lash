//! Object descriptors packed directly into directory leaves. Identity
//! edges are semantic data, never storage graph child references.
use super::heap_impl::{PagedHeap, census_fault, keyed};
use super::tree::{Access, Cell, SealWork, Tree, grant};
use super::{
    FamilyRecord, FrozenRef, HeapWriteError, MembershipRun, ObjectKind, SealBudget, SemanticHeap,
    TransientRoots,
};
use lash_kernel_doc::Site;
use lash_kernel_doc::graph_tokens::{TokenCodec, TokenError, TokenReader, TokenWriter};
use lash_kernel_doc::heap::*;
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
        captures: Tree<EntityId>,
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
    16u64.saturating_add(match value {
        Value::Int(IntRef::Inline(v)) => v.bits().div_ceil(8),
        Value::Int(IntRef::Paged { summary, .. }) => summary.bits.div_ceil(8),
        Value::Text(v) => v.summary.utf8_bytes,
        Value::Bytes(v) => v.len,
        Value::Timestamp(v) => v.nanoseconds.bits().div_ceil(8),
        Value::Tuple(v) => v.summary.cost.0,
        Value::Error(v) => v.cost.0,
        Value::Function(v) => v.as_str().len() as u64,
        Value::Handle(v) => (v.kind.len() + v.id.len()) as u64,
        _ => 0,
    })
}
impl Cell for Value {
    fn cost(&self) -> u64 {
        value_cost(self)
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
    fn prepare(&mut self, access: &Access, work: &mut SealWork) -> Result<bool, HeapFault> {
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
            value_children(&e.data, out);
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
            value_grants(Arc::make_mut(&mut e.data), children)?;
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
    use crate::paged::*;
    use lash_kernel_doc::{Name, TaskId, TaskIdentity};

    #[derive(Clone)]
    pub struct FramePage {
        pub statement: Site,
        pub bindings: Tree<BindingRecord>,
        pub loops: Tree<LoopRecord>,
        pub finally: Tree<FinallyRecord>,
        pub iterated: Tree<Value>,
        pub departing: Tree<Value>,
        pub arguments: Option<Tree<Value>>,
    }
    #[derive(Clone)]
    pub struct TaskPage {
        pub identity: TaskIdentity,
        pub state: TaskStateRecord,
        pub result_retained: bool,
        pub joiners: Tree<JoinerRecord>,
        pub observed: bool,
        pub passed: bool,
        pub failed: bool,
        pub occurrences: Tree<u64>,
        pub frames: Tree<FramePage>,
    }
    impl FramePage {
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
    impl Cell for BindingRecord {
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
    impl Cell for FramePage {
        fn cost(&self) -> u64 {
            64 + self.bindings.cost()
                + self.iterated.cost()
                + self.departing.cost()
                + self.arguments.as_ref().map_or(0, Tree::cost)
        }
        fn prepare(&mut self, access: &Access, work: &mut SealWork) -> Result<bool, HeapFault> {
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
    impl Cell for TaskPage {
        fn cost(&self) -> u64 {
            self.frames.cost()
                + match &self.state {
                    TaskStateRecord::Resuming(
                        IncomingRecord::Value(v) | IncomingRecord::Raise(v),
                    ) => value_cost(v),
                    _ => 0,
                }
        }
        fn prepare(&mut self, access: &Access, work: &mut SealWork) -> Result<bool, HeapFault> {
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
    impl TokenCodec for FramePage {
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
                iterated: <Tree<Value>>::decode(input)?,
                departing: <Tree<Value>>::decode(input)?,
                arguments: <Option<Tree<Value>>>::decode(input)?,
            })
        }
    }

    impl TokenCodec for TaskPage {
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
                frames: <Tree<FramePage>>::decode(input)?,
            })
        }
    }
}

impl Cell for MembershipRun {
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
        self.reserve_logical(
            frozen.summary.cost,
            &TransientRoots {
                registers: &[],
                pinned: &[],
                native_args: &[],
                fresh: &[],
                reserved: LogicalBytes(0),
            },
        )?;
        let family = self.next_family()?;
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
        if let Some(census) = &mut self.census {
            census
                .added(frozen.summary.cost, frozen.summary.entities)
                .map_err(census_fault)?;
            self.accounting = census.certificate();
        } else {
            self.accounting.upper.0 = self
                .accounting
                .upper
                .0
                .checked_add(frozen.summary.cost.0)
                .ok_or_else(|| HeapFault::Malformed("family cost overflow".into()))?;
            self.accounting.entities = self
                .accounting
                .entities
                .checked_add(frozen.summary.entities)
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
            return Err(HeapFault::Malformed("sweep named an absent family".into()));
        };
        let mut runs = Tree::<MembershipRun>::open(
            family.membership.page,
            family.membership.len,
            family.membership.cost,
            false,
        );
        let rank = runs.passed(&ordinal_key(entity.ordinal), &self.access)?;
        let Some((key, run)) = rank
            .checked_sub(1)
            .map(|i| runs.rank_get(i, &self.access))
            .transpose()?
            .flatten()
        else {
            return Err(HeapFault::Malformed("family membership has a hole".into()));
        };
        if !run.live || entity.ordinal - run.start >= run.len {
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
