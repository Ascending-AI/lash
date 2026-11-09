//! Immutable heads (cutover contract §6, astra §1): the certificate a row
//! names an immutable object by, how an object opens from one, and how a
//! decoded body is checked against it before the run sees it.

use lash_kernel_doc::Integer;
use lash_kernel_doc::Value;
use lash_kernel_doc::arena::native::NativeError;
use lash_kernel_doc::arena::{
    ArenaHandle, Body, Bytes, Chunk, Chunks, ColdImmutable, LongText, Sequence,
};
use lash_kernel_doc::heap::{EntityId, IntSummary, NormalKey};

use super::sizes::composite;
use super::{Arena, Facts, Head, NO_PAGE, Summary, has_cold, keys, malformed, tables};

impl Arena {
    /// The one handle of durable immutable `entity`, which a row names
    /// with `head`: the one the run already has, whose head must be
    /// `head`, else a proxy true to it. A function and a host handle open
    /// resident, as does an empty tuple; any other opens cold, its head's
    /// summaries in its body and facts, and every read of its payload
    /// answers [`lash_kernel_doc::Retry::Cold`].
    ///
    /// # Errors
    ///
    /// Two heads for one object: rows that disagree about it.
    pub fn open_immutable(
        &mut self,
        entity: EntityId,
        head: &Head,
    ) -> Result<ArenaHandle, NativeError> {
        if let Some(handle) = self.entities.get(&entity).copied() {
            let facts = self.facts(handle)?;
            if let Some(held) = facts.head.get() {
                return if held == head {
                    Ok(handle)
                } else {
                    Err(malformed("two heads certify one immutable"))
                };
            }
            // A proxy a ref named first takes the body its head gives.
            if facts.cold && matches!(self.body(handle)?, Body::Cell(Value::Absent)) {
                let (body, summary) = opened(head);
                let cold = has_cold(&body);
                *self
                    .objects
                    .get_mut(handle)
                    .ok_or_else(|| malformed("a stale handle reached the arena"))? = body;
                let facts = self.facts_mut(handle)?;
                facts.cold = cold;
                facts.summary = summary;
                let _ = facts.head.set(head.clone());
                if !cold {
                    self.colds = self.colds.saturating_sub(1);
                }
                return Ok(handle);
            }
            // An object of this run, named by a row for the first time.
            return if self.head(handle)? == *head {
                self.certify(handle, head.clone())?;
                Ok(handle)
            } else {
                Err(malformed("two heads certify one immutable"))
            };
        }
        let (body, summary) = opened(head);
        let facts = Facts {
            entity: Some(entity),
            cold: has_cold(&body),
            based: true,
            summary,
            ..Facts::default()
        };
        let _ = facts.head.set(head.clone());
        Ok(self.insert(body, facts))
    }

    /// The head of the immutable `handle`: the one it holds, else the one
    /// its resident body gives. A tuple's normal form reads its members'
    /// heads where they have them, never their bodies.
    ///
    /// # Errors
    ///
    /// [`lash_kernel_doc::Retry::Cold`] for a member a tuple's normal form
    /// needs and has no head of; a malformed fault for a mutable object.
    pub fn head(&self, handle: ArenaHandle) -> Result<Head, NativeError> {
        let facts = self.facts(handle)?;
        if let Some(head) = facts.head.get() {
            return Ok(head.clone());
        }
        let body = self.body(handle)?;
        let value = match body {
            Body::Tuple(_) => Value::Tuple(handle),
            Body::BigInt(_) => Value::BigInt(handle),
            Body::Timestamp(_) => Value::BigTimestamp(handle),
            Body::LongText(_) => Value::LongText(handle),
            Body::Bytes(_) => Value::Bytes(handle),
            _ => Value::Absent,
        };
        head_of(body, facts.summary, || {
            keys::normal_key(&self.objects, &self.facts, &value)
        })
    }

    /// The head of the immutable `body`, which a load decoded and has not
    /// installed: what [`Arena::head`] gives once it is, so a load checks a
    /// row's body against the head every referrer names it by before the
    /// run sees it.
    ///
    /// # Errors
    ///
    /// A malformed fault for a mutable or partial body.
    pub fn detached_head(&self, body: &Body) -> Result<Head, NativeError> {
        let shape = match body {
            Body::Tuple(sequence) => composite(
                &self.facts,
                sequence
                    .chunks
                    .values()
                    .ok_or_else(|| malformed("a decoded tuple is partial"))?,
            ),
            Body::Error(error) => composite(&self.facts, std::slice::from_ref(&error.data)),
            _ => Summary::default(),
        };
        head_of(body, shape, || {
            keys::detached_normal(&self.objects, &self.facts, body)
        })
    }

    /// Fixes `head` as the immutable `handle`'s, as a seal that wrote a row
    /// naming it does: every later row names it so.
    pub fn certify(&self, handle: ArenaHandle, head: Head) -> Result<(), NativeError> {
        let _ = self.facts(handle)?.head.set(head);
        Ok(())
    }

    /// Re-keys every resident map and set once `handle`, which a key named
    /// by its token, has a durable id: a ref key means its referent's id
    /// from then on, as it will on pages and after a reopen. Rare: only a
    /// ref keyed before its referent's first seal costs it.
    pub(super) fn rekey_after(&mut self, handle: ArenaHandle) -> Result<(), NativeError> {
        if !self.facts(handle)?.token_keyed.replace(false) {
            return Ok(());
        }
        for index in 0..self.objects.capacity() as u32 {
            let Some(table) = self.objects.handle_at(index) else {
                continue;
            };
            let (Body::Map(body) | Body::Set(body)) = self.body(table)? else {
                continue;
            };
            let refs = (0..body.chunks.count())
                .filter_map(|chunk| body.chunks.resident(chunk))
                .flat_map(|values| values.iter().step_by(2))
                .any(|key| matches!(key, Value::ObjectRef(_) | Value::Tuple(_)));
            let Some(held) = refs
                .then(|| self.facts[index as usize].index.take())
                .flatten()
            else {
                continue;
            };
            let (Body::Map(body) | Body::Set(body)) = self.body(table)? else {
                unreachable!("the table's body was just read")
            };
            let rekeyed =
                tables::rekeyed(&self.objects, &self.facts, &self.layouts, table, body, held)?;
            self.facts[index as usize].index = Some(rekeyed);
        }
        Ok(())
    }
}

/// The body an immutable opens with from its `head`, and its summary: a
/// function and a host handle whole, as is an empty tuple; any other cold,
/// holding what its head says of it. What a load of the object's row
/// replaces, and what it is admitted against.
#[must_use]
pub fn opened(head: &Head) -> (Body, Summary) {
    fn cold<T>(len: u64) -> Chunk<T> {
        Chunk::Cold {
            page: NO_PAGE,
            len: u32::try_from(len).unwrap_or(u32::MAX),
        }
    }
    match head {
        Head::Tuple { len: 0, shape, .. } => (Body::Tuple(Sequence::whole(Vec::new())), *shape),
        Head::Tuple { len, shape, .. } => (
            Body::Tuple(Sequence {
                len: *len,
                chunks: Chunks::Split(vec![cold(*len)]),
            }),
            *shape,
        ),
        Head::Error(shape) => (Body::Cold(ColdImmutable::Error), *shape),
        Head::BigInt { summary, .. } => (
            Body::Cold(ColdImmutable::BigInt(*summary)),
            Summary::default(),
        ),
        Head::Timestamp { summary, .. } => (
            Body::Cold(ColdImmutable::Timestamp(*summary)),
            Summary::default(),
        ),
        Head::LongText { summary, .. } => (
            Body::LongText(LongText {
                summary: *summary,
                chunks: Box::new([cold(summary.utf8_bytes)]),
            }),
            Summary::default(),
        ),
        Head::Bytes { len, .. } => (
            Body::Bytes(Bytes {
                len: *len,
                chunks: Box::new([cold(*len)]),
            }),
            Summary::default(),
        ),
        Head::Function(callable) => (Body::Function(Box::new(*callable)), Summary::default()),
        Head::Handle(host) => (Body::Handle(host.clone()), Summary::default()),
    }
}

/// The head of the immutable `body`, whose summary is `shape` and whose
/// normal form `normal` gives.
fn head_of(
    body: &Body,
    shape: Summary,
    normal: impl FnOnce() -> Result<Option<NormalKey>, NativeError>,
) -> Result<Head, NativeError> {
    let int = |int: &Integer| IntSummary {
        bits: int.bits(),
        negative: int.is_negative(),
    };
    let key = |normal: Option<NormalKey>| {
        normal.ok_or_else(|| malformed("an immutable scalar is no legal key"))
    };
    Ok(match body {
        Body::Tuple(sequence) => Head::Tuple {
            len: sequence.len,
            shape,
            normal: normal()?,
        },
        Body::Error(_) => Head::Error(shape),
        Body::BigInt(value) => Head::BigInt {
            summary: int(value),
            normal: key(normal()?)?,
        },
        Body::Timestamp(value) => Head::Timestamp {
            summary: int(value),
            normal: key(normal()?)?,
        },
        Body::LongText(text) => Head::LongText {
            summary: text.summary,
            normal: key(normal()?)?,
        },
        Body::Bytes(bytes) => Head::Bytes {
            len: bytes.len,
            normal: key(normal()?)?,
        },
        Body::Function(callable) => Head::Function(**callable),
        Body::Handle(host) => Head::Handle(host.clone()),
        Body::Cold(_) => return Err(malformed("a cold immutable has no head")),
        _ => return Err(malformed("a mutable object has no head")),
    })
}
