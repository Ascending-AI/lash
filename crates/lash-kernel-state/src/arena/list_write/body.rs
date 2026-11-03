//! One detached body shared by every private alias and ordered overwrite.
use super::*;
use crate::kernel_internals::PagedHeap;
use crate::paged::logical::{
    BoundaryTransaction, Ledger, Member, MemberEdit, MemberFault, Owner, RootValue,
};

pub struct PreparedMemberBody {
    version: ContentVersion,
    handle: ArenaHandle,
    shadow: Shadow,
    edits: Vec<MemberEdit>,
    original: Vec<Value>,
    body: Option<Body>,
    index: Option<tables::Index>,
    flat: Option<sizes::FlatSize>,
    cards: Vec<u64>,
    resident_before: u64,
}
enum Shadow {
    Cell(Value),
    List(Vec<Value>),
    Table {
        record: bool,
        set: bool,
        values: Vec<Value>,
        keys: Vec<Key>,
        sequences: Vec<u64>,
        next_sequence: u64,
    },
}
impl Arena {
    /// Snapshot once. Cold, structural and otherwise unsupported operations
    /// remain exact source boundaries; reservation changes no guest content.
    pub fn prepare_member_body(
        &mut self,
        handle: ArenaHandle,
    ) -> Result<Option<super::PreparedMemberBody>, HeapFault> {
        let Some(version) = self.content_version(handle) else {
            return Ok(None);
        };
        if !self.facts(handle).is_ok_and(|facts| !facts.cold) {
            return Ok(None);
        }
        let original: Vec<Value>;
        let shadow = match self.body(handle) {
            Ok(Body::Cell(value)) => {
                original = copy(&[*value])?;
                Shadow::Cell(*value)
            }
            Ok(Body::List(_)) => {
                let Ok(values) = self.sequence(handle) else {
                    return Ok(None);
                };
                original = copy(values)?;
                Shadow::List(copy(&original)?)
            }
            Ok(Body::Map(_) | Body::Set(_) | Body::Record(_)) => {
                let (table, index) = self.table_view(handle).map_err(body_fault)?;
                let Ok(values) = tables::pairs(handle, table) else {
                    return Ok(None);
                };
                original = copy(values)?;
                let mut keys = Vec::new();
                reserve(&mut keys, values.len() / 2)?;
                for pair in values.as_chunks::<2>().0 {
                    keys.push(self.key(&pair[0]).map_err(body_fault)?);
                }
                Shadow::Table {
                    record: matches!(self.body(handle), Ok(Body::Record(_))),
                    set: matches!(self.body(handle), Ok(Body::Set(_))),
                    values: copy(&original)?,
                    keys,
                    sequences: {
                        let mut sequences = Vec::new();
                        reserve(&mut sequences, values.len() / 2)?;
                        sequences.extend(index.sequences(0).iter());
                        sequences
                    },
                    next_sequence: index.next(),
                }
            }
            _ => return Ok(None),
        };
        let resident_before = body_values(self.body(handle).map_err(body_fault)?);
        Ok(Some(PreparedMemberBody {
            version,
            handle,
            shadow,
            edits: Vec::new(),
            original,
            body: None,
            index: None,
            flat: None,
            cards: Vec::new(),
            resident_before,
        }))
    }
}
impl PreparedMemberBody {
    pub fn handle(&self) -> ArenaHandle {
        self.handle
    }
    pub fn is_current(&self, arena: &Arena) -> bool {
        arena.content_is(&self.version)
    }
    fn position(&self, arena: &Arena, member: &Member) -> Result<Option<usize>, HeapFault> {
        Ok(match (&self.shadow, member) {
            (Shadow::Cell(_), Member::Variable) => Some(0),
            (Shadow::List(values), Member::List(index)) => {
                usize::try_from(*index).ok().filter(|index| {
                    let len = match &self.body {
                        Some(Body::List(sequence)) => sequence.len as usize,
                        _ => values.len(),
                    };
                    *index < len
                })
            }
            (Shadow::Table { keys, record, .. }, Member::Field(name)) if *record => {
                let key = Key::of_text(name);
                keys.iter().position(|held| held == &key)
            }
            (Shadow::Table { keys, .. }, Member::Table(value)) => {
                let key = arena.key(value).map_err(body_fault)?;
                keys.iter().position(|held| held == &key)
            }
            _ => None,
        })
    }
    /// Every alias reads the same detached member, including prior writes.
    pub fn read(&self, arena: &Arena, member: &Member) -> Result<Option<Value>, HeapFault> {
        let Some(at) = self.position(arena, member)? else {
            return Ok(None);
        };
        Ok(Some(match &self.shadow {
            Shadow::Cell(value) => *value,
            Shadow::List(values) => match &self.body {
                Some(Body::List(sequence)) => sequence
                    .chunks
                    .resident(0)
                    .and_then(|values| values.get(at))
                    .copied()
                    .ok_or_else(|| {
                        HeapFault::Malformed("sealed private list lost its value".into())
                    })?,
                _ => values[at],
            },
            Shadow::Table { values, .. } => values[2 * at + 1],
        }))
    }
    /// Fold a source-ordered overwrite. Structural edits hand off before
    /// touching the shadow, except the original list's single append slot.
    pub fn stage(
        &mut self,
        arena: &Arena,
        member: &Member,
        value: Value,
    ) -> Result<bool, HeapFault> {
        if self.body.is_some() || !self.is_current(arena) {
            return Ok(false);
        }
        let at = self.position(arena, member)?;
        let card = match (&mut self.shadow, member, at) {
            (Shadow::Cell(held), Member::Variable, Some(_)) => {
                *held = value;
                None
            }
            (Shadow::List(values), Member::List(index), at) => {
                let Ok(index) = usize::try_from(*index) else {
                    return Ok(false);
                };
                if let Some(at) = at {
                    values[at] = value;
                } else if index == values.len() && index == self.original.len() {
                    reserve(values, 1)?;
                    values.push(value);
                } else {
                    return Ok(false);
                }
                Some(index as u64 / CARD)
            }
            (
                Shadow::Table {
                    values,
                    set,
                    sequences,
                    ..
                },
                _,
                Some(at),
            ) => {
                if *set && !matches!(value, Value::Absent) {
                    return Ok(false);
                }
                values[2 * at + 1] = value;
                Some(sequences[at] / CARD)
            }
            _ => return Ok(false),
        };
        // Equal normalized keys name one canonical member even when a later
        // source operand uses another spelling, such as 1 and 1.0.
        let position = self.position(arena, member)?;
        let existing = self
            .edits
            .iter()
            .position(|edit| self.position(arena, &edit.member).ok().flatten() == position);
        if let Some(at) = existing {
            self.edits[at].next = Some(value);
        } else {
            reserve(&mut self.edits, 1)?;
            self.edits.push(MemberEdit {
                handle: self.handle,
                member: member.clone(),
                next: Some(value),
            });
        }
        if let Some(card) = card
            && !self.cards.contains(&card)
        {
            reserve(&mut self.cards, 1)?;
            self.cards.push(card);
        }
        Ok(true)
    }
    pub fn is_sealed(&self) -> bool {
        self.body.is_some()
    }
    pub fn edits(&self) -> &[MemberEdit] {
        &self.edits
    }
    /// Build derived table storage once, after the selected private prefix.
    /// All fallible reservations precede source and logical admission.
    pub fn seal(mut self, arena: &mut Arena) -> Result<Option<Self>, HeapFault> {
        if !self.is_current(arena) || self.edits.is_empty() {
            return Ok(None);
        }
        if self.body.is_some() {
            return Ok(Some(self));
        }
        let body = match &mut self.shadow {
            Shadow::Cell(value) => Body::Cell(*value),
            Shadow::List(values) => Body::List(Sequence::whole(std::mem::take(values))),
            Shadow::Table {
                values,
                record,
                set,
                sequences,
                next_sequence,
                ..
            } => {
                let (table, mut index) = tables::build(
                    &arena.objects,
                    &arena.facts,
                    &arena.layouts,
                    values
                        .as_chunks::<2>()
                        .0
                        .iter()
                        .map(|[key, value]| (*key, *value)),
                )
                .map_err(body_fault)?;
                index
                    .resequence(std::mem::take(sequences), *next_sequence)
                    .map_err(body_fault)?;
                self.index = Some(index);
                if *record {
                    Body::Record(table)
                } else if *set {
                    Body::Set(table)
                } else {
                    Body::Map(table)
                }
            }
        };
        self.flat = sizes::summarize_flat(&arena.objects, &body);
        self.body = Some(body);
        reserve(&mut arena.zero, self.original.len())?;
        reserve(&mut arena.dirty, 1)?;
        reserve(
            &mut arena.facts[self.handle.index() as usize].cards,
            self.cards.len(),
        )?;
        Ok(Some(self))
    }
    /// Check the actual final target, addresses and values independently of
    /// their prices. In particular an equal-price foreign cell is not this body.
    pub fn matches_edits(&self, edits: &[MemberEdit]) -> bool {
        edits.len() == self.edits.len()
            && edits.iter().zip(&self.edits).all(|(actual, expected)| {
                actual.handle == expected.handle
                    && actual.member.same(&expected.member)
                    && actual
                        .next
                        .zip(expected.next)
                        .is_some_and(|(a, b)| a.same(&b))
            })
    }
    /// Revalidate the original snapshot before projecting its final edits.
    /// Source, observer, charge and interior-peak proof remain the caller's.
    pub fn prepare_boundary<'a>(
        &self,
        ledger: &Ledger,
        mut registration: Registration<'a>,
        heap: &PagedHeap,
        inputs: &[Owner],
        outputs: &[(Owner, RootValue<'_>)],
    ) -> Result<Option<BoundaryTransaction<'a>>, MemberFault> {
        if !self.is_sealed() || !self.is_current(registration.arena()) {
            return Ok(None);
        }
        ledger
            .prepare_source_member_bodies_boundary(
                registration,
                heap,
                &[self.edits()],
                inputs,
                outputs,
            )
            .map(Some)
    }
    /// The storage owner installs one body, after checking the exact final
    /// edits and the same entry content version under accepted registration.
    pub fn publish(self, arena: &mut Arena, edits: Vec<MemberEdit>) {
        assert!(arena.registration_active() && self.is_current(arena));
        assert!(self.matches_edits(&edits));
        let Some(body) = self.body else {
            unreachable!("a member body is sealed before acceptance");
        };
        // Retain all final edges before releasing any old edge, including
        // self references and a child shared by several changed members.
        arena.hold_members(&body);
        for value in &self.original {
            arena.unhold(value);
        }
        arena.resident = arena
            .resident
            .saturating_sub(self.resident_before)
            .saturating_add(body_values(&body));
        let Some(held) = arena.objects.get_mut(self.handle) else {
            unreachable!("checked member body");
        };
        *held = body;
        arena.facts[self.handle.index() as usize].flat = self.flat;
        if let Some(index) = self.index {
            arena.facts[self.handle.index() as usize].index = Some(index);
        }
        if self.cards.is_empty() {
            Arena::mark_resident(
                &mut arena.facts[self.handle.index() as usize],
                &mut arena.dirty,
                self.handle,
                None,
            );
        } else {
            for card in self.cards {
                Arena::mark_resident(
                    &mut arena.facts[self.handle.index() as usize],
                    &mut arena.dirty,
                    self.handle,
                    Some(card),
                );
            }
        }
    }
}
fn body_fault(error: NativeError) -> HeapFault {
    match error {
        NativeError::Host(error) => error,
        error => HeapFault::Malformed(error.to_string()),
    }
}

fn copy(values: &[Value]) -> Result<Vec<Value>, HeapFault> {
    let mut copied = Vec::new();
    reserve(&mut copied, values.len())?;
    copied.extend_from_slice(values);
    Ok(copied)
}

impl Arena {
    /// All bodies in an atomic publication share these physical work queues.
    /// Reserving each snapshot separately would reserve only the largest body.
    pub(crate) fn reserve_member_body_publication(
        &mut self,
        handles: impl Iterator<Item = ArenaHandle>,
    ) -> Result<(), HeapFault> {
        let mut slots = 0_usize;
        let mut bodies = 0_usize;
        for handle in handles {
            let values = usize::try_from(body_values(self.body(handle).map_err(body_fault)?))
                .map_err(|_| HeapFault::Exhausted {
                    resource: HostResource::StagingBytes,
                })?;
            slots = slots.checked_add(values).ok_or(HeapFault::Exhausted {
                resource: HostResource::StagingBytes,
            })?;
            bodies = bodies.checked_add(1).ok_or(HeapFault::Exhausted {
                resource: HostResource::StagingBytes,
            })?;
        }
        reserve(&mut self.zero, slots)?;
        reserve(&mut self.dirty, bodies)
    }
}
