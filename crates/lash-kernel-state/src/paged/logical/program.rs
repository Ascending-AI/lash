//! Canonical construction payloads and native graph facts. Production
//! publication uses ledger boundary transactions without an event journal.
use super::*;
mod native;
#[cfg(any(test, feature = "perf-witness"))]
mod oracle;
use crate::arena::{Arena, Registration};
use crate::paged::PagedHeap;
use lash_kernel_doc::{Value, heap::PageValue};
#[cfg(any(test, feature = "perf-witness"))]
pub use oracle::{
    AcceptedProgram, EvaluatedProgram, JournalEvent, Materializations, PreparedProgram, Program,
};
use std::collections::BTreeSet;

/// Canonical source payloads. Their direct occurrence prices use Holding and
/// LogicalHeldV1; the executor supplies no byte delta or entity header price.
#[derive(Clone)]
pub enum VirtualBody {
    List(Vec<PageValue>),
    /// Evaluated resident members retain their native provenance and identities.
    NativeList(Vec<Value>),
    NativeMap(Vec<(Value, Value)>),
    NativeSet(Vec<Value>),
    NativeRecord(Vec<(Value, Value)>),
    Variable(PageValue),
    /// A fresh shared variable retains the evaluated RHS provenance.
    NativeVariable(Value),
    Closure(Vec<EntityId>),
    Map(Vec<(PageValue, PageValue)>),
    Set(Vec<PageValue>),
    Record(Vec<(PageValue, PageValue)>),
}
impl VirtualBody {
    /// Canonical direct member obligations, excluding the entity header.
    pub fn member_bytes(&self, heap: &PagedHeap, arena: &mut Arena) -> Result<u64, HeapFault> {
        self.holding(heap, arena)?
            .bytes
            .checked_sub(Cost::OBJECT)
            .ok_or_else(|| invalid("virtual member price underflow"))
    }
    pub(super) fn holding(
        &self,
        heap: &PagedHeap,
        arena: &mut Arena,
    ) -> Result<Holding, HeapFault> {
        use lash_kernel_doc::heap::PageReader;
        let access = heap.pages();
        let mut body = Holding::scalar(Cost::OBJECT);
        let mut add = |holding: Holding, field: bool| -> Result<(), HeapFault> {
            let bytes = holding
                .bytes
                .checked_sub(if field { Cost::REFERENCE } else { 0 })
                .ok_or_else(|| invalid("virtual field price underflow"))?;
            body.bytes = body
                .bytes
                .checked_add(bytes)
                .ok_or_else(|| invalid("virtual body cost overflow"))?;
            holding.each_edge(access, &mut |edge| {
                body.add_edge(edge.target, edge.count, access)
            })
        };
        match self {
            Self::NativeList(values) => {
                for value in values {
                    add(Holding::from_native(arena, access, value)?, false)?;
                }
            }
            Self::NativeMap(entries) | Self::NativeRecord(entries) => {
                let record = matches!(self, Self::NativeRecord(_));
                let mut keys = BTreeSet::new();
                for (key, value) in entries {
                    if record && !matches!(key, Value::Text(_) | Value::LongText(_)) {
                        return Err(invalid("record field is not text"));
                    }
                    let normal = arena
                        .semantics()
                        .normal_key(key)
                        .map_err(|error| invalid(&error.to_string()))?
                        .ok_or_else(|| invalid("virtual table key is not a key"))?;
                    if !keys.insert(normal) {
                        return Err(invalid("virtual table key occurs twice"));
                    }
                    add(Holding::from_native(arena, access, key)?, record)?;
                    add(Holding::from_native(arena, access, value)?, false)?;
                }
            }
            Self::NativeSet(values) => {
                let mut keys = BTreeSet::new();
                for key in values {
                    let normal = arena
                        .semantics()
                        .normal_key(key)
                        .map_err(|error| invalid(&error.to_string()))?
                        .ok_or_else(|| invalid("virtual set key is not a key"))?;
                    if !keys.insert(normal) {
                        return Err(invalid("virtual set key occurs twice"));
                    }
                    add(Holding::from_native(arena, access, key)?, false)?;
                    add(Holding::from_native(arena, access, &Value::Absent)?, false)?;
                }
            }
            Self::List(values) => {
                for value in values {
                    add(Holding::from_page(access, value)?, false)?;
                }
            }
            Self::Variable(value) => add(Holding::from_page(access, value)?, false)?,
            Self::NativeVariable(value) => add(Holding::from_native(arena, access, value)?, false)?,
            Self::Closure(captures) => {
                for entity in captures {
                    add(
                        Holding::with_edges(
                            Cost::ENTITY_EDGE,
                            [Identity::Object(*entity)],
                            access,
                        )?,
                        false,
                    )?;
                }
            }
            Self::Map(entries) | Self::Record(entries) => {
                let record = matches!(self, Self::Record(_));
                let mut keys = BTreeSet::new();
                for (key, value) in entries {
                    if record && !matches!(key, PageValue::Text(_)) {
                        return Err(invalid("record field is not text"));
                    }
                    let digest = heap
                        .normal_key(key)?
                        .ok_or_else(|| invalid("virtual table key is not a key"))?;
                    if !keys.insert(digest) {
                        return Err(invalid("virtual table key occurs twice"));
                    }
                    add(Holding::from_page(access, key)?, record)?;
                    add(Holding::from_page(access, value)?, false)?;
                }
            }
            Self::Set(values) => {
                let mut keys = BTreeSet::new();
                for key in values {
                    let digest = heap
                        .normal_key(key)?
                        .ok_or_else(|| invalid("virtual table key is not a key"))?;
                    if !keys.insert(digest) {
                        return Err(invalid("virtual set key occurs twice"));
                    }
                    add(Holding::from_page(access, key)?, false)?;
                    add(Holding::from_page(access, &PageValue::Null)?, false)?;
                }
            }
        }
        Ok(body)
    }
}
#[derive(Clone)]
pub struct VirtualEntity {
    pub entity: EntityId,
    pub body: VirtualBody,
}
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum FrameEvent {
    Enter { task: TaskId, invocation: u64 },
    Leave { task: TaskId, invocation: u64 },
}
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ScratchEvent {
    Begin { owner: Owner, bytes: u64 },
    End { owner: Owner },
}
impl Ledger {
    pub fn next_invocation(&self) -> u64 {
        self.state.next_invocation
    }
}

impl PublicationValue {
    pub fn native_holding(
        &self,
        heap: &PagedHeap,
        arena: &Arena,
        owners: &BTreeMap<EntityId, lash_kernel_doc::NativeResourceOwner>,
    ) -> Result<lash_kernel_doc::NativeResourceHolding, HeapFault> {
        use lash_kernel_doc::NativeResourceOrigin as Origin;
        let (bytes, entity, alias) = match self {
            Self::Native(value) => (
                arena.logical_local(value)?,
                value
                    .handle()
                    .and_then(|handle| arena.facts(handle).ok()?.ledger_entity()),
                matches!(
                    value,
                    Value::List(_)
                        | Value::Map(_)
                        | Value::Set(_)
                        | Value::Record(_)
                        | Value::Closure(_)
                ),
            ),
            Self::Page(value) => {
                let entity = match value {
                    PageValue::List(id)
                    | PageValue::Map(id)
                    | PageValue::Set(id)
                    | PageValue::Record(id)
                    | PageValue::Closure(id) => Some(*id),
                    _ => None,
                };
                (
                    Holding::from_page(heap.pages(), value)?.bytes(),
                    entity,
                    entity.is_some(),
                )
            }
            _ => return Err(invalid("native result provenance is unavailable")),
        };
        let origin = if alias {
            entity
                .and_then(|id| owners.get(&id).copied())
                .map_or(Origin::Alias, Origin::Fresh)
        } else {
            Origin::Immutable
        };
        Ok(lash_kernel_doc::NativeResourceHolding { bytes, origin })
    }
}
impl VirtualBody {
    pub fn native_members(
        &self,
        heap: &PagedHeap,
        arena: &Arena,
        owners: &BTreeMap<EntityId, lash_kernel_doc::NativeResourceOwner>,
    ) -> Result<
        (
            lash_kernel_doc::NativeResourceEntity,
            Vec<lash_kernel_doc::NativeResourceHolding>,
        ),
        HeapFault,
    > {
        use lash_kernel_doc::NativeResourceEntity as Kind;
        let mut members = Vec::new();
        let mut add = |value: PublicationValue| -> Result<(), HeapFault> {
            members.push(value.native_holding(heap, arena, owners)?);
            Ok(())
        };
        let kind = match self {
            Self::NativeList(values) => {
                for value in values {
                    add(PublicationValue::Native(*value))?;
                }
                Kind::List
            }
            Self::NativeSet(values) => {
                for value in values {
                    add(PublicationValue::Native(*value))?;
                    add(PublicationValue::Native(Value::Absent))?;
                }
                Kind::Set
            }
            Self::NativeMap(entries) => {
                for (key, value) in entries {
                    add(PublicationValue::Native(*key))?;
                    add(PublicationValue::Native(*value))?;
                }
                Kind::Map
            }
            Self::NativeRecord(entries) => {
                let mut names = 0_u64;
                for (key, value) in entries {
                    let bytes = arena
                        .semantics()
                        .text(key)
                        .map_err(|error| invalid(&error.to_string()))?
                        .len() as u64;
                    names = names
                        .checked_add(bytes)
                        .ok_or_else(|| invalid("record name overflow"))?;
                    add(PublicationValue::Native(*value))?;
                }
                Kind::Record {
                    field_name_bytes: names,
                }
            }
            Self::List(values) => {
                for value in values {
                    add(PublicationValue::Page(value.clone()))?;
                }
                Kind::List
            }
            Self::Set(values) => {
                for value in values {
                    add(PublicationValue::Page(value.clone()))?;
                    add(PublicationValue::Page(PageValue::Null))?;
                }
                Kind::Set
            }
            Self::Variable(value) => {
                add(PublicationValue::Page(value.clone()))?;
                Kind::Variable
            }
            Self::NativeVariable(value) => {
                add(PublicationValue::Native(*value))?;
                Kind::Variable
            }
            Self::Closure(captures) => {
                let members = captures
                    .iter()
                    .map(|id| {
                        let mut holding =
                            PublicationValue::Page(PageValue::Ref(Identity::Object(*id)))
                                .native_holding(heap, arena, owners)?;
                        holding.bytes = Cost::ENTITY_EDGE;
                        Ok(holding)
                    })
                    .collect::<Result<Vec<_>, HeapFault>>()?;
                return Ok((Kind::Closure, members));
            }
            Self::Map(entries) => {
                for (key, value) in entries {
                    add(PublicationValue::Page(key.clone()))?;
                    add(PublicationValue::Page(value.clone()))?;
                }
                Kind::Map
            }
            Self::Record(entries) => {
                let mut names = 0_u64;
                for (key, value) in entries {
                    let PageValue::Text(text) = key else {
                        return Err(invalid("record field is not text"));
                    };
                    names = names
                        .checked_add(text.summary.utf8_bytes)
                        .ok_or_else(|| invalid("record name overflow"))?;
                    add(PublicationValue::Page(value.clone()))?;
                }
                Kind::Record {
                    field_name_bytes: names,
                }
            }
        };
        Ok((kind, members))
    }
}
