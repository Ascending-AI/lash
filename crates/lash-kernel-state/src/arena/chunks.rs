//! Large objects by chunk (seam D of `docs/design/kernel-end-state.md`,
//! swap 2.4(b)): the arena's side of opening a proxy's head, decoding one
//! chunk, joining resident chunks, resolving a key through the heap's key
//! index, and dropping clean large objects back to their pages.
//!
//! The machine does the reading, outside any native call
//! (`lash-kernel-vm`'s `machine/project.rs`): it asks which chunks are
//! cold ([`Arena::cold_chunks`], [`Arena::payload_chunk`]), reads them from
//! the heap, and hands the decoded contents back here.

use std::ops::Range;

use lash_kernel_doc::arena::{ArenaHandle, Body, Bytes, Chunk, Chunks, LongText, Sequence, Table};
use lash_kernel_doc::heap::{NormalKey, PAYLOAD_LEAF_BYTES, PageRef};
use lash_kernel_doc::{NativeError, Value};

use super::tables::{self, Index};
use super::{Arena, Key, NO_PAGE, ProxyKind, body_values, first_cold, malformed};

/// What a cold chunk holds, for the machine to read from the heap.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ColdChunk {
    /// Positions `start .. start + count` of a list.
    List { start: u64, count: u64 },
    /// The entries of a table whose insertion sequences are in `span`.
    Table { span: Range<u64> },
}

/// A node of a long text's or bytes' payload tree, decoded.
#[derive(Debug)]
pub enum PayloadPart {
    /// A leaf's bytes: the chunk becomes resident.
    Leaf(Vec<u8>),
    /// An index node's children, each a page and its byte length: the chunk
    /// becomes one cold chunk per child.
    Index(Vec<(PageRef, u64)>),
}

/// Values held resident before the arena drops clean large objects back to
/// their pages at a statement boundary: 64 MiB of values.
pub(super) const EVICT_AT: u64 = 1 << 22;

fn cold<T>(len: u64) -> Chunk<T> {
    Chunk::Cold {
        page: NO_PAGE,
        len: u32::try_from(len).unwrap_or(u32::MAX),
    }
}

fn sequence_mut(body: &mut Body) -> Result<&mut Sequence, NativeError> {
    match body {
        Body::List(sequence) => Ok(sequence),
        _ => Err(malformed("a list handle names another body")),
    }
}

fn table_mut(body: &mut Body) -> Result<&mut Table, NativeError> {
    match body {
        Body::Map(table) | Body::Set(table) | Body::Record(table) => Ok(table),
        _ => Err(malformed("a table handle names another body")),
    }
}

impl Arena {
    /// Whether a proxy, or the chunks of `handle`, wait to be decoded.
    fn recount(&mut self, handle: ArenaHandle, before: bool) -> Result<(), NativeError> {
        let after = self.needs_decode(handle)?;
        match (before, after) {
            (true, false) => self.colds = self.colds.saturating_sub(1),
            (false, true) => self.colds = self.colds.saturating_add(1),
            _ => {}
        }
        Ok(())
    }

    /// Opens a proxy's head as a large list of `len` values: one cold chunk
    /// per [`Sequence::CHUNK`] positions.
    pub fn open_list(&mut self, handle: ArenaHandle, len: u64) -> Result<(), NativeError> {
        let width = Sequence::CHUNK as u64;
        let chunks = Chunks::Split(
            (0..len.div_ceil(width))
                .map(|chunk| cold(width.min(len - chunk * width)))
                .collect(),
        );
        let (body, facts) = self.parts_mut(handle)?;
        *body = Body::List(Sequence { len, chunks });
        facts.cold = false;
        Ok(())
    }

    /// Opens a proxy's head as a large table of `kind` (a map, a set or a
    /// record) with `len` entries below sequence `next`: chunks spanning
    /// equal ranges of sequences, each cold.
    pub fn open_table(
        &mut self,
        handle: ArenaHandle,
        kind: ProxyKind,
        len: u64,
        next: u64,
    ) -> Result<(), NativeError> {
        let width = tables::width(len, next);
        let count = next.div_ceil(width) as usize;
        let table = Table {
            len,
            chunks: Chunks::Split((0..count).map(|_| cold(0)).collect()),
        };
        let body = match kind {
            ProxyKind::Map => Body::Map(table),
            ProxyKind::Set => Body::Set(table),
            ProxyKind::Record => Body::Record(table),
            _ => return Err(malformed("a table proxy of another kind")),
        };
        let (held, facts) = self.parts_mut(handle)?;
        *held = body;
        facts.cold = false;
        facts.index = Some(Index::chunked(width, count, next));
        Ok(())
    }

    /// Up to `count` cold chunks of the list or table `handle`, from chunk
    /// `from` on: what the machine reads next. A resident chunk among them
    /// is skipped.
    pub fn cold_chunks(
        &self,
        handle: ArenaHandle,
        from: u32,
        count: u32,
    ) -> Result<Vec<(u32, ColdChunk)>, NativeError> {
        let width = Sequence::CHUNK as u64;
        let mut out = Vec::new();
        match self.body(handle)? {
            Body::List(sequence) => {
                for (at, chunk) in sequence.chunks.iter().enumerate().skip(from as usize) {
                    if out.len() as u32 == count {
                        break;
                    }
                    if let Chunk::Cold { len, .. } = chunk {
                        out.push((
                            at as u32,
                            ColdChunk::List {
                                start: at as u64 * width,
                                count: u64::from(len),
                            },
                        ));
                    }
                }
            }
            Body::Map(table) | Body::Set(table) | Body::Record(table) => {
                let index = self
                    .facts(handle)?
                    .index
                    .as_ref()
                    .ok_or_else(|| malformed("a table has no index"))?;
                for (at, chunk) in table.chunks.iter().enumerate().skip(from as usize) {
                    if out.len() as u32 == count {
                        break;
                    }
                    if matches!(chunk, Chunk::Cold { .. }) {
                        out.push((
                            at as u32,
                            ColdChunk::Table {
                                span: index.span(at),
                            },
                        ));
                    }
                }
            }
            _ => {}
        }
        Ok(out)
    }

    /// Chunk `chunk` of the list `handle`, decoded: its values, resident.
    pub fn decoded_list(
        &mut self,
        handle: ArenaHandle,
        chunk: u32,
        values: Vec<Value>,
    ) -> Result<(), NativeError> {
        let before = self.needs_decode(handle)?;
        let added = values.len() as u64;
        let members: Vec<ArenaHandle> = values.iter().filter_map(Value::handle).collect();
        let sequence = sequence_mut(
            self.objects
                .get_mut(handle)
                .ok_or_else(|| malformed("a stale handle reached the machine"))?,
        )?;
        let Chunks::Split(chunks) = &mut sequence.chunks else {
            return Err(malformed("a decoded list chunk is not cold"));
        };
        match chunks.get_mut(chunk as usize) {
            Some(held @ Chunk::Cold { .. }) => {
                if let Chunk::Cold { len, .. } = held
                    && u64::from(*len) != added
                {
                    return Err(malformed("a list chunk's rows disagree with its length"));
                }
                *held = Chunk::Resident(values);
            }
            _ => return Err(malformed("a decoded list chunk is not cold")),
        }
        for member in members {
            self.hold_handle(member);
        }
        self.resident = self.resident.saturating_add(added);
        self.recount(handle, before)
    }

    /// Chunk `chunk` of the table `handle`, decoded: its entries in
    /// sequence order, each with its sequence and its key's normal form.
    pub fn decoded_table(
        &mut self,
        handle: ArenaHandle,
        chunk: u32,
        entries: Vec<(u64, Value, Value, NormalKey)>,
    ) -> Result<(), NativeError> {
        let before = self.needs_decode(handle)?;
        let added = entries.len() as u64 * 2;
        let (body, facts) = self.parts_mut(handle)?;
        let index = facts
            .index
            .as_mut()
            .ok_or_else(|| malformed("a table has no index"))?;
        let table = table_mut(body)?;
        let Chunks::Split(chunks) = &mut table.chunks else {
            return Err(malformed("a decoded table chunk is not cold"));
        };
        let Some(held @ Chunk::Cold { .. }) = chunks.get_mut(chunk as usize) else {
            return Err(malformed("a decoded table chunk is not cold"));
        };
        let mut pairs = Vec::with_capacity(entries.len() * 2);
        let mut seqs = Vec::with_capacity(entries.len());
        for (seq, key, value, normal) in entries {
            index.resolved(Key::normalized(&key, normal), Some(seq));
            seqs.push(seq);
            pairs.push(key);
            pairs.push(value);
        }
        index.decoded(chunk as usize, seqs)?;
        let members: Vec<ArenaHandle> = pairs.iter().filter_map(Value::handle).collect();
        *held = Chunk::Resident(pairs);
        for member in members {
            self.hold_handle(member);
        }
        self.resident = self.resident.saturating_add(added);
        self.recount(handle, before)
    }

    /// Joins the chunks of `handle` into one once every chunk is resident,
    /// so a whole read of it borrows one slice; gives whether it did.
    pub fn join_chunks(&mut self, handle: ArenaHandle) -> Result<bool, NativeError> {
        let (body, facts) = self.parts_mut(handle)?;
        match body {
            Body::List(sequence) | Body::Tuple(sequence) => return Ok(sequence.chunks.join()),
            Body::Map(table) | Body::Set(table) | Body::Record(table) => {
                if !table.chunks.join() {
                    return Ok(false);
                }
                facts
                    .index
                    .as_mut()
                    .ok_or_else(|| malformed("a table has no index"))?
                    .join();
            }
            Body::LongText(text) | Body::LongFunction(text)
                if text.chunks.len() > 1 && first_cold(&text.chunks).is_none() =>
            {
                let mut joined = String::with_capacity(text.summary.utf8_bytes as usize);
                for chunk in &text.chunks {
                    if let Chunk::Resident(part) = chunk {
                        joined.push_str(part);
                    }
                }
                text.chunks = Box::new([Chunk::Resident(joined)]);
            }
            Body::Bytes(bytes) if bytes.chunks.len() > 1 && first_cold(&bytes.chunks).is_none() => {
                let mut joined = Vec::with_capacity(bytes.len as usize);
                for chunk in &bytes.chunks {
                    if let Chunk::Resident(part) = chunk {
                        joined.extend_from_slice(part);
                    }
                }
                bytes.chunks = Box::new([Chunk::Resident(joined.into_boxed_slice())]);
            }
            _ => return Ok(false),
        }
        Ok(true)
    }

    /// The first cold chunk of `handle`, or 0 for a proxy or an object
    /// whose chunks are all resident.
    pub fn first_cold_chunk(&self, handle: ArenaHandle) -> Result<u32, NativeError> {
        Ok(match self.body(handle)? {
            Body::List(sequence) | Body::Tuple(sequence) => sequence.chunks.first_cold(),
            Body::Map(table) | Body::Set(table) | Body::Record(table) => table.chunks.first_cold(),
            Body::LongText(text) | Body::LongFunction(text) => first_cold(&text.chunks),
            Body::Bytes(bytes) => first_cold(&bytes.chunks),
            _ => None,
        }
        .unwrap_or(0))
    }

    /// How many chunks the body of `handle` is in.
    pub fn chunk_count(&self, handle: ArenaHandle) -> Result<usize, NativeError> {
        Ok(match self.body(handle)? {
            Body::List(sequence) | Body::Tuple(sequence) => sequence.chunks.count(),
            Body::Map(table) | Body::Set(table) | Body::Record(table) => table.chunks.count(),
            Body::LongText(text) | Body::LongFunction(text) => text.chunks.len(),
            Body::Bytes(bytes) => bytes.chunks.len(),
            _ => 0,
        })
    }

    /// The bytes cold chunk `chunk` of a long text or bytes spans in its
    /// payload, or `None` when it is resident or no such chunk.
    pub fn payload_chunk(
        &self,
        handle: ArenaHandle,
        chunk: u32,
    ) -> Result<Option<Range<u64>>, NativeError> {
        fn span<T>(chunks: &[Chunk<T>], len: impl Fn(&T) -> u64, chunk: u32) -> Option<Range<u64>> {
            let mut offset = 0u64;
            for (at, held) in chunks.iter().enumerate() {
                let bytes = match held {
                    Chunk::Resident(part) => len(part),
                    Chunk::Cold { len, .. } => u64::from(*len),
                };
                if at as u32 == chunk {
                    return matches!(held, Chunk::Cold { .. }).then_some(offset..offset + bytes);
                }
                offset += bytes;
            }
            None
        }
        Ok(match self.body(handle)? {
            Body::LongText(text) | Body::LongFunction(text) => {
                span(&text.chunks, |part| part.len() as u64, chunk)
            }
            Body::Bytes(bytes) => span(&bytes.chunks, |part| part.len() as u64, chunk),
            _ => None,
        })
    }

    /// Chunk `chunk` of a long text or bytes, decoded as `part`: a leaf
    /// becomes resident; an index node becomes its children, cold.
    pub fn decoded_payload(
        &mut self,
        handle: ArenaHandle,
        chunk: u32,
        part: PayloadPart,
    ) -> Result<(), NativeError> {
        fn splice<T>(
            chunks: &mut Box<[Chunk<T>]>,
            chunk: usize,
            with: Vec<Chunk<T>>,
        ) -> Result<(), NativeError> {
            if !matches!(chunks.get(chunk), Some(Chunk::Cold { .. })) {
                return Err(malformed("a decoded payload chunk is not cold"));
            }
            let mut all: Vec<Chunk<T>> = std::mem::take(chunks).into_vec();
            all.splice(chunk..=chunk, with);
            *chunks = all.into_boxed_slice();
            Ok(())
        }
        let before = self.needs_decode(handle)?;
        let body = self
            .objects
            .get_mut(handle)
            .ok_or_else(|| malformed("a stale handle reached the machine"))?;
        let mut added = 0u64;
        match (body, part) {
            (Body::LongText(text) | Body::LongFunction(text), PayloadPart::Leaf(bytes)) => {
                added = bytes.len() as u64 / 16;
                let text_part =
                    String::from_utf8(bytes).map_err(|_| malformed("a text chunk is not UTF-8"))?;
                splice(
                    &mut text.chunks,
                    chunk as usize,
                    vec![Chunk::Resident(text_part)],
                )?;
            }
            (Body::Bytes(held), PayloadPart::Leaf(bytes)) => {
                added = bytes.len() as u64 / 16;
                splice(
                    &mut held.chunks,
                    chunk as usize,
                    vec![Chunk::Resident(bytes.into_boxed_slice())],
                )?;
            }
            (Body::LongText(text) | Body::LongFunction(text), PayloadPart::Index(children)) => {
                let children = children
                    .into_iter()
                    .map(|(page, len)| Chunk::Cold {
                        page,
                        len: u32::try_from(len).unwrap_or(u32::MAX),
                    })
                    .collect();
                splice(&mut text.chunks, chunk as usize, children)?;
            }
            (Body::Bytes(held), PayloadPart::Index(children)) => {
                let children = children
                    .into_iter()
                    .map(|(page, len)| Chunk::Cold {
                        page,
                        len: u32::try_from(len).unwrap_or(u32::MAX),
                    })
                    .collect();
                splice(&mut held.chunks, chunk as usize, children)?;
            }
            _ => return Err(malformed("a payload chunk of another body")),
        }
        self.resident = self.resident.saturating_add(added);
        self.recount(handle, before)
    }

    /// The normal form of `key` when the table `handle` must look it up in
    /// the heap's key index first: it has a cold chunk and its index does
    /// not know the key. A proxy answers [`lash_kernel_doc::Retry::Cold`].
    #[inline]
    pub fn unresolved(
        &self,
        handle: ArenaHandle,
        key: &Value,
    ) -> Result<Option<NormalKey>, NativeError> {
        if self.colds == 0 {
            // No object has a cold chunk or is a proxy.
            return Ok(None);
        }
        self.warm(handle)?;
        if let Body::Map(table) | Body::Set(table) | Body::Record(table) = self.body(handle)?
            && table.chunks.first_cold().is_none()
        {
            return Ok(None);
        }
        if !self.unresolved_key(handle, &self.key(key)?)? {
            return Ok(None);
        }
        Ok(Some(
            self.semantics()
                .normal_key(key)?
                .ok_or_else(super::invalid_key)?,
        ))
    }

    /// Whether the table `handle` must look `key` up in the heap's key
    /// index first.
    #[inline]
    pub fn unresolved_key(&self, handle: ArenaHandle, key: &Key) -> Result<bool, NativeError> {
        if self.colds == 0 {
            return Ok(false);
        }
        self.warm(handle)?;
        let cold = match self.body(handle)? {
            Body::Map(table) | Body::Set(table) | Body::Record(table) => {
                table.chunks.first_cold().is_some()
            }
            _ => return Ok(false),
        };
        Ok(cold
            && self
                .facts(handle)?
                .index
                .as_ref()
                .is_some_and(|index| index.unknown(key, cold)))
    }

    /// Records what the heap's key index holds for `key` in the table
    /// `handle`: the entry's sequence, or none.
    pub fn resolved(
        &mut self,
        handle: ArenaHandle,
        key: Key,
        seq: Option<u64>,
    ) -> Result<(), NativeError> {
        self.facts_mut(handle)?
            .index
            .as_mut()
            .ok_or_else(|| malformed("a table has no index"))?
            .resolved(key, seq);
        Ok(())
    }

    /// How many chunks the machine decodes for this read of `handle`: one
    /// for the first since the last statement boundary, then twice as many
    /// each time, so a scan of a cold object decodes it in logarithmically
    /// many reads.
    pub fn decode_count(&mut self, handle: ArenaHandle) -> Result<u32, NativeError> {
        let facts = self.facts_mut(handle)?;
        let count = 1u32 << facts.streak.min(20);
        facts.streak = facts.streak.saturating_add(1);
        if facts.streak == 1 {
            self.streaked.push(handle);
        }
        Ok(count)
    }

    /// A statement boundary: decoding streaks end, and once the arena holds
    /// [`EVICT_AT`] values resident, every clean large object the heap holds
    /// drops back to its pages, to be decoded chunk by chunk again when a
    /// read reaches it; the next drop waits until residency doubles. Call
    /// only where no borrow of a body is live.
    ///
    /// Most boundaries have neither to do, and cost the two tests.
    #[inline]
    pub fn statement_boundary(&mut self) {
        if !self.streaked.is_empty() || self.resident >= self.evict_at {
            self.end_streaks_and_evict();
        }
    }

    #[cold]
    #[inline(never)]
    fn end_streaks_and_evict(&mut self) {
        // The list keeps its buffer for the next statement's streaks.
        let mut streaked = std::mem::take(&mut self.streaked);
        for handle in streaked.drain(..) {
            if let Some(facts) = self.facts.get_mut(handle.index() as usize) {
                facts.streak = 0;
            }
        }
        self.streaked = streaked;
        if self.resident >= self.evict_at {
            self.evict_clean();
            self.evict_at = self.resident.saturating_mul(2).max(EVICT_AT);
        }
    }

    /// Drops every clean large object the heap holds back to its pages: a
    /// list or a table of more than one chunk, and a long text or bytes on
    /// payload pages past one leaf.
    fn evict_clean(&mut self) {
        for index in 0..self.facts.len() {
            let Some(handle) = self.objects.handle_at(index as u32) else {
                continue;
            };
            let facts = &self.facts[index];
            let large = match self.objects.get(handle) {
                Some(Body::List(sequence)) => sequence.len > Sequence::CHUNK as u64,
                Some(Body::Map(table) | Body::Set(table) | Body::Record(table)) => {
                    table.len > Sequence::CHUNK as u64
                }
                Some(Body::LongText(text)) => text.summary.utf8_bytes > PAYLOAD_LEAF_BYTES,
                Some(Body::Bytes(bytes)) => bytes.len > PAYLOAD_LEAF_BYTES,
                _ => false,
            };
            let paged = match facts.entity {
                Some(_) => facts.based && !facts.dirty && !facts.cold,
                None => facts.page.get().is_some_and(|page| {
                    matches!(
                        page,
                        lash_kernel_doc::heap::PageValue::Text(lash_kernel_doc::heap::TextRef {
                            body: lash_kernel_doc::heap::Payload::Paged(_),
                            ..
                        }) | lash_kernel_doc::heap::PageValue::Bytes(
                            lash_kernel_doc::heap::BytesRef {
                                body: lash_kernel_doc::heap::Payload::Paged(_),
                                ..
                            }
                        )
                    )
                }),
            };
            if large && paged {
                let _ = self.drop_to_pages(handle);
            }
        }
    }

    /// Puts the clean large object `handle` back on its pages: a list or a
    /// table becomes a proxy of its kind, a long text or bytes one cold
    /// chunk of its payload. What its body held is held no longer.
    fn drop_to_pages(&mut self, handle: ArenaHandle) -> Result<(), NativeError> {
        let before = self.needs_decode(handle)?;
        let body = match self.body(handle)? {
            Body::List(_) => ProxyKind::List.placeholder(),
            Body::Map(_) => ProxyKind::Map.placeholder(),
            Body::Set(_) => ProxyKind::Set.placeholder(),
            Body::Record(_) => ProxyKind::Record.placeholder(),
            Body::LongText(text) => Body::LongText(LongText {
                summary: text.summary,
                chunks: Box::new([cold(text.summary.utf8_bytes)]),
            }),
            Body::Bytes(bytes) => Body::Bytes(Bytes {
                len: bytes.len,
                chunks: Box::new([cold(bytes.len)]),
            }),
            _ => return Ok(()),
        };
        let held = std::mem::replace(
            self.objects
                .get_mut(handle)
                .ok_or_else(|| malformed("a stale handle reached the machine"))?,
            body,
        );
        self.resident = self
            .resident
            .saturating_sub(body_values(&held))
            .saturating_add(1);
        self.unhold_members(&held);
        let facts = self.facts_mut(handle)?;
        if facts.entity.is_some() {
            facts.cold = true;
            facts.index = None;
        }
        self.recount(handle, before)
    }
}
