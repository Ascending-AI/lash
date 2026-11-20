//! Immutable heads (cutover contract §6, astra §1): the certificate a row
//! names an immutable object by, how an object opens from one, and how a
//! decoded body is checked against it before the run sees it.

use lash_kernel_doc::Integer;
use lash_kernel_doc::Value;
use lash_kernel_doc::arena::native::NativeError;
use lash_kernel_doc::arena::{ArenaHandle, Body, ColdImmutable};
use lash_kernel_doc::heap::{EntityId, IntSummary, NormalKey};

use super::sizes::named_composite;
use super::{Arena, Facts, Head, Summary, has_cold, keys, malformed, tables};

impl Arena {
    /// The one handle of durable immutable `entity`, which a row names
    /// with `head`: the one the run already has, whose head or pending
    /// claim must equal `head`, else a proxy true to it. A function and
    /// a host handle its head holds whole open resident. Any other opens
    /// opaque until its own row vouches for the claim ([`Self::vouch`]):
    /// no fact of the head is published before, and every read of it
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
            if let Some(held) = facts.named_by() {
                return if held == head {
                    Ok(handle)
                } else {
                    Err(malformed("two heads certify one immutable"))
                };
            }
            // A proxy a ref named first takes the body its head gives.
            if facts.cold && matches!(self.body(handle)?, Body::Cell(Value::Absent)) {
                facts.try_projection()?;
                let (body, summary) = opened(head);
                let cold = has_cold(&body);
                *self
                    .objects
                    .get_mut(handle)
                    .ok_or_else(|| malformed("a stale handle reached the arena"))? = body;
                let facts = self.facts_mut(handle)?;
                facts.cold = cold;
                facts.summary = summary;
                named(facts.try_projection_mut()?, head);
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
        let mut facts = Facts {
            cold: has_cold(&body),
            summary,
            ..Facts::default()
        };
        let projected = facts.try_projection_mut()?;
        projected.entity = Some(entity);
        projected.based = true;
        named(projected, head);
        Ok(self.insert(body, facts))
    }

    /// Publish the head (and a tuple's or error's summary) of an immutable
    /// after its own row has matched the referrers' claim
    /// ([`Facts::named_by`]).
    ///
    /// # Errors
    ///
    /// A malformed fault for a stale handle.
    pub fn vouch(&mut self, handle: ArenaHandle) -> Result<(), NativeError> {
        let facts = self.facts_mut(handle)?;
        let Some(claim) = facts
            .projected_mut()
            .and_then(|projected| projected.claim.take())
        else {
            return Ok(());
        };
        if let Some(shape) = composite_shape(&claim) {
            facts.summary = shape;
        }
        let projected = facts.try_projection()?;
        let _ = projected.head.set(claim);
        Ok(())
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
        if let Some(head) = facts.named_by() {
            return Ok(head.clone());
        }
        keys::content_head(
            &self.objects,
            &self.facts,
            Some(handle),
            self.body(handle)?,
            facts.summary,
        )
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
            Body::Tuple(sequence) => named_composite(
                &self.facts,
                sequence
                    .chunks
                    .values()
                    .ok_or_else(|| malformed("a decoded tuple is partial"))?,
            ),
            Body::Error(error) => named_composite(&self.facts, std::slice::from_ref(&error.data)),
            _ => Summary::default(),
        };
        keys::content_head(&self.objects, &self.facts, None, body, shape)
    }

    /// Fixes `head` as the immutable `handle`'s, as a seal that wrote a row
    /// naming it does: every later row names it so.
    /// A pending claim stays pending when the seal copies it. Only the
    /// object's own row vouches for it.
    pub fn certify(&self, handle: ArenaHandle, head: Head) -> Result<(), NativeError> {
        let facts = self.facts(handle)?;
        if let Some(projected) = facts.projected()
            && let Some(claim) = &projected.claim
        {
            return if *claim == head {
                Ok(())
            } else {
                Err(malformed(
                    "a seal certifies another head than the immutable's claim",
                ))
            };
        }
        super::freeze(self.body(handle)?);
        let projected = facts.try_projection()?;
        let _ = projected.head.set(head);
        projected.named_head.take();
        Ok(())
    }

    /// Forgets every head that read `handle`'s identity, now that it has
    /// changed (a durable id given, or withdrawn by a registration that
    /// did not commit), and every head that read one of those: each is
    /// computed again when it is next asked for.
    pub(super) fn unhead(&self, handle: ArenaHandle) {
        let mut pending = vec![handle];
        while let Some(named) = pending.pop() {
            let Some(projected) = self
                .facts
                .get(named.index() as usize)
                .and_then(Facts::projected)
            else {
                continue;
            };
            for dependent in projected.headed_by.take() {
                if self
                    .facts
                    .get(dependent.index() as usize)
                    .and_then(Facts::projected)
                    .is_some_and(|projected| projected.named_head.take().is_some())
                {
                    pending.push(dependent);
                }
            }
        }
    }

    /// Re-keys every resident map and set once `handle`, which a key named
    /// by its token, has a durable id: a ref key means its referent's id
    /// from then on, as it will on pages and after a reopen. Rare: only a
    /// ref keyed before its referent's first seal costs it. The object
    /// stays marked, so a registration that withdraws the id re-keys the
    /// tables back ([`Self::rekey`]).
    pub(super) fn rekey_after(&mut self, handle: ArenaHandle) -> Result<(), NativeError> {
        if self.facts(handle)?.token_keyed.get() {
            self.rekey()?;
        }
        Ok(())
    }

    /// Re-keys every resident map and set holding a ref or tuple key by
    /// the identities its keys' referents have now.
    pub(super) fn rekey(&mut self) -> Result<(), NativeError> {
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

/// Gives the immutable whose projection is `projected` the `head` a row
/// names it by. Tuples and errors keep a claim until their own rows
/// vouch for it ([`Arena::vouch`]). Other bodies check their heads when
/// their payloads are read.
/// A row's head for an object it opens: a claim until the object's own
/// row vouches for it ([`Arena::vouch`]), but a function's or a host
/// handle's, whose head is its whole value.
fn named(projected: &mut super::Projected, head: &Head) {
    if matches!(head, Head::Function(_) | Head::Handle { .. }) {
        let _ = projected.head.set(head.clone());
    } else {
        projected.claim = Some(head.clone());
    }
}

pub(super) fn composite_shape(head: &Head) -> Option<Summary> {
    match head {
        Head::Tuple { shape, .. } | Head::Error { shape, .. } => Some(*shape),
        _ => None,
    }
}

/// The body an immutable opens with from its `head`, and its summary.
/// Every immutable but a function and a host handle opens opaque: no
/// fact of its head is in its body. A row load replaces this body and
/// admits the growth relative to it.
#[must_use]
pub fn opened(head: &Head) -> (Body, Summary) {
    let body = match head {
        Head::Tuple { .. } => Body::Cold(ColdImmutable::Tuple),
        Head::Error { .. } => Body::Cold(ColdImmutable::Error),
        Head::BigInt { .. } => Body::Cold(ColdImmutable::BigInt),
        Head::Timestamp { .. } => Body::Cold(ColdImmutable::Timestamp),
        Head::LongText { .. } => Body::Cold(ColdImmutable::LongText),
        Head::Bytes { .. } => Body::Cold(ColdImmutable::Bytes),
        Head::Function(callable) => Body::Function(Box::new(*callable)),
        Head::Handle {
            kind_len, id_len, ..
        } => match head.resident_handle() {
            Some(host) => Body::Handle(host),
            None => Body::Cold(ColdImmutable::Handle {
                kind_len: *kind_len,
                id_len: *id_len,
            }),
        },
    };
    (body, Summary::default())
}

/// The head of the immutable scalar `body`, whose normal form `normal`
/// gives; a tuple's and an error's are [`keys::content_head`]'s.
pub(super) fn head_of(
    body: &Body,
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
        Body::Handle(host) => Head::handle(host),
        Body::Tuple(_) | Body::Error(_) => {
            return Err(malformed("a tuple's or an error's head is its content's"));
        }
        Body::Cold(_) => return Err(malformed("a cold immutable has no head")),
        _ => return Err(malformed("a mutable object has no head")),
    })
}
