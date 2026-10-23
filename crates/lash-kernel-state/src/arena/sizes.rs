//! Ephemeral immutable reach facts and flat charge-size facts. Charge
//! summaries retain a wider exact accumulator: a saturated answer can later
//! shrink, so subtracting from a saturated u64 would undercharge.
use super::{Arena, Facts};
use lash_kernel_doc::arena::{ArenaHandle, Body};
use lash_kernel_doc::{NativeError, Value};

/// The summary an immutable composite keeps: what its page form's
/// descriptor certifies (`TupleSummary`, `ErrorRef`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Summary {
    /// The tuple and error nesting depth inside it, itself included
    /// (`K-VAL-034`).
    pub depth: u32,
    pub task_edges: u64,
    /// Whether a member at any depth names an object or a task.
    pub has_edges: bool,
}

pub(super) fn value_summary(facts: &[Facts], value: &Value) -> Summary {
    match value {
        Value::Tuple(handle) | Value::Error(handle) => facts
            .get(handle.index() as usize)
            .map_or_else(Summary::default, |facts| facts.summary),
        Value::List(_)
        | Value::Map(_)
        | Value::Set(_)
        | Value::Record(_)
        | Value::Closure(_)
        | Value::ObjectRef(_) => Summary {
            has_edges: true,
            ..Summary::default()
        },
        Value::Task(_) | Value::TaskRef(_) => Summary {
            task_edges: 1,
            has_edges: true,
            ..Summary::default()
        },
        _ => Summary::default(),
    }
}

/// The summary of an immutable composite of `members`.
pub(super) fn composite(facts: &[Facts], members: &[Value]) -> Summary {
    let mut summary = Summary {
        depth: 1,
        ..Summary::default()
    };
    for member in members {
        let inner = value_summary(facts, member);
        summary.depth = summary.depth.max(inner.depth.saturating_add(1));
        summary.task_edges = summary.task_edges.saturating_add(inner.task_edges);
        summary.has_edges |= inner.has_edges;
    }
    summary
}

/// Three u64 words keep exact leaf sizes and eligibility, including while a
/// composite member makes the summary unusable. With the Option discriminant
/// this costs 32 bytes, bounded by two value slots. No descendant dependencies
/// are needed: composite members contribute a marker until replaced or removed.
#[derive(Clone, Copy, Debug)]
pub(super) struct FlatSize {
    low: u64,
    high: u64,
    nested: u64,
}

#[derive(Clone, Copy)]
enum MemberSize {
    Leaf(u64),
    Nested,
}
impl MemberSize {
    fn parts(self) -> (u128, u64) {
        match self {
            Self::Leaf(size) => (u128::from(size), 0),
            Self::Nested => (0, 1),
        }
    }
}
impl FlatSize {
    fn new(sum: u128, nested: u64) -> Self {
        Self {
            low: sum as u64,
            high: (sum >> 64) as u64,
            nested,
        }
    }
    fn get(self) -> u128 {
        u128::from(self.low) | (u128::from(self.high) << 64)
    }
}

impl Arena {
    /// A flat graph's exact K-CHG-005 size. No allocation or mutation on a
    /// miss: arbitrary sharing, immutable composites and cycles use the VM's
    /// single seen-set walker. In particular a tuple may hold mutable children.
    pub fn flat_charge_size(&self, value: &Value) -> Result<Option<u64>, NativeError> {
        let handle = match value {
            Value::Tuple(h) | Value::List(h) | Value::Map(h) | Value::Set(h) | Value::Record(h) => {
                *h
            }
            _ => return Ok(None),
        };
        // Check generation and kind before trusting the slot's summary.
        if !matches!(
            (value, self.body(handle)?),
            (Value::List(_), Body::List(_))
                | (Value::Tuple(_), Body::Tuple(_))
                | (Value::Map(_), Body::Map(_))
                | (Value::Set(_), Body::Set(_))
                | (Value::Record(_), Body::Record(_))
        ) {
            return Ok(None);
        }
        let facts = self.facts(handle)?;
        if facts.cold {
            return Ok(None);
        }
        let Some(sum) = facts.flat else {
            return Ok(None);
        };
        if sum.nested != 0 {
            return Ok(None);
        }
        let len = self.len(handle)?;
        let extent = if matches!(value, Value::Map(_)) {
            u128::from(len) * 2
        } else {
            u128::from(len)
        };
        Ok(Some(
            (1 + extent + sum.get()).min(u128::from(u64::MAX)) as u64
        ))
    }

    /// Only leaf values qualify. An immutable outer composite does not prove
    /// its descendants immutable, and shared entities must be counted once.
    fn flat_member_size(&self, value: &Value) -> Option<MemberSize> {
        #[cfg(any(test, feature = "perf-witness"))]
        crate::perf_witness::record(|work| work.flat_size_members += 1);
        let extent = match value {
            Value::Int(word) => u64::from(*word != 0),
            Value::Text(text) => text.as_bytes().len() as u64,
            Value::BigInt(handle) => match self.body(*handle).ok()? {
                Body::BigInt(integer) => integer.bits().div_ceil(64),
                _ => return None,
            },
            Value::LongText(handle) => match self.body(*handle).ok()? {
                Body::LongText(text) => text.summary.utf8_bytes,
                _ => return None,
            },
            Value::Bytes(handle) => match self.body(*handle).ok()? {
                Body::Bytes(bytes) => bytes.len,
                _ => return None,
            },
            Value::Tuple(_)
            | Value::List(_)
            | Value::Map(_)
            | Value::Set(_)
            | Value::Record(_)
            | Value::Error(_) => return Some(MemberSize::Nested),
            _ => 0,
        };
        Some(MemberSize::Leaf(extent.saturating_add(1)))
    }

    pub(super) fn summarize_flat(&self, body: &Body) -> Option<FlatSize> {
        let (values, skip, step) = match body {
            Body::List(sequence) | Body::Tuple(sequence) => (sequence.chunks.values()?, 0, 1),
            Body::Map(table) => (table.chunks.values()?, 0, 1),
            Body::Set(table) => (table.chunks.values()?, 0, 2),
            Body::Record(table) => (table.chunks.values()?, 1, 2),
            _ => return None,
        };
        // A resident slice has at most isize::MAX / size_of::<Value>()
        // members, each at most u64::MAX: their sum fits u128 exactly.
        values
            .iter()
            .skip(skip)
            .step_by(step)
            .try_fold(FlatSize::new(0, 0), |sum, value| {
                let (size, nested) = self.flat_member_size(value)?.parts();
                Some(FlatSize::new(sum.get() + size, sum.nested + nested))
            })
    }

    pub(super) fn rebuild_flat(&mut self, handle: ArenaHandle) -> Result<(), NativeError> {
        let sum = self.summarize_flat(self.body(handle)?);
        self.facts_mut(handle)?.flat = sum;
        Ok(())
    }

    pub(super) fn update_table_flat(
        &mut self,
        handle: ArenaHandle,
        old: Option<(&Value, &Value)>,
        new: Option<(&Value, &Value)>,
    ) {
        match self.objects.get(handle) {
            Some(Body::Map(_)) => {
                self.update_flat(handle, old.map(|pair| pair.0), new.map(|pair| pair.0));
                self.update_flat(handle, old.map(|pair| pair.1), new.map(|pair| pair.1));
            }
            Some(Body::Set(_)) => {
                self.update_flat(handle, old.map(|pair| pair.0), new.map(|pair| pair.0))
            }
            Some(Body::Record(_)) => {
                self.update_flat(handle, old.map(|pair| pair.1), new.map(|pair| pair.1))
            }
            _ => {}
        }
    }

    pub(super) fn update_flat(
        &mut self,
        handle: ArenaHandle,
        old: Option<&Value>,
        new: Option<&Value>,
    ) {
        let Some(sum) = self.facts[handle.index() as usize].flat else {
            return;
        };
        let old = old.map_or(Some(MemberSize::Leaf(0)), |value| {
            self.flat_member_size(value)
        });
        let new = new.map_or(Some(MemberSize::Leaf(0)), |value| {
            self.flat_member_size(value)
        });
        self.facts[handle.index() as usize].flat = old.zip(new).map(|(old, new)| {
            let (old_size, old_nested) = old.parts();
            let (new_size, new_nested) = new.parts();
            FlatSize::new(
                sum.get() - old_size + new_size,
                sum.nested - old_nested + new_nested,
            )
        });
    }
}

/// Whether a durable identity names this mutable body.
pub(super) fn is_mutable(body: &Body) -> bool {
    matches!(
        body,
        Body::List(_)
            | Body::Map(_)
            | Body::Set(_)
            | Body::Record(_)
            | Body::Closure { .. }
            | Body::Cell(_)
    )
}
