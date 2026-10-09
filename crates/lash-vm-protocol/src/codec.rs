//! Length-framed encoding and bounded decoding.
//!
//! A frame is a fixed header and a MessagePack payload:
//!
//! ```text
//! magic "LVMP" (4) | payload length, u32 big-endian (4) | payload
//! ```
//!
//! Decoding refuses, with a typed [`CodecRefusal`] and before allocating for
//! the payload:
//!
//! - a declared frame over [`DecodeLimits::max_frame_bytes`];
//! - a frame shorter than its header or declared length (truncated);
//! - a payload whose MessagePack structure nests deeper than
//!   [`DecodeLimits::max_depth`], holds more than [`DecodeLimits::max_nodes`]
//!   values, or declares more than [`DecodeLimits::max_allocation_bytes`] of
//!   strings, byte strings and container elements in total;
//! - a payload that is not exactly one well-formed message.
//!
//! The bounds are charged by a structural walk over the payload that reads
//! only declared lengths, so a hostile length is refused before the typed
//! decode allocates anything for it.

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::message::{ParentFrame, WorkerFrame};
use crate::outcome::Detail;

pub const FRAME_MAGIC: [u8; 4] = *b"LVMP";

/// Magic and payload length.
pub const FRAME_HEADER_BYTES: usize = 4 + 4;

/// What one decoded container element may cost the typed decode, charged
/// against [`DecodeLimits::max_allocation_bytes`] before it is allocated: an
/// upper bound on the in-memory size of any element type the message schema
/// holds in a sequence or map.
pub const CONTAINER_ELEMENT_CHARGE: u64 = 128;

/// The decode bounds of one frame.
///
/// The charge schedule: every MessagePack value is one node, map keys
/// included, so a map of `n` entries charges `2n` nodes; every string and
/// byte string charges its declared length; every container charges
/// [`CONTAINER_ELEMENT_CHARGE`] per value it declares. All of it is charged
/// from declared lengths before the typed decode allocates.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DecodeLimits {
    pub max_frame_bytes: u32,
    pub max_depth: u32,
    pub max_nodes: u64,
    pub max_allocation_bytes: u64,
}

impl DecodeLimits {
    /// The FIG-4157 measured presets: 4 MiB frames, including the envelope;
    /// 100,000 charged nodes, about twice the densest measured continuation's
    /// 50,044; 64 MiB of cumulative charged allocation, about 3.6 times the
    /// largest measured decode's 18.6 MB; and depth 128, far above the
    /// protocol's own nesting, since VM state crosses as one opaque byte
    /// string.
    pub const fn standard() -> Self {
        Self {
            max_frame_bytes: 4 * 1024 * 1024,
            max_depth: 128,
            max_nodes: 100_000,
            max_allocation_bytes: 64 * 1024 * 1024,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Error, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CodecRefusal {
    #[error("frame is truncated: {needed} bytes needed, {available} available")]
    Truncated { needed: u64, available: u64 },
    #[error("frame does not start with the protocol magic")]
    BadMagic,
    #[error("frame declares {declared} bytes, over the {limit}-byte bound")]
    FrameTooLarge { limit: u64, declared: u64 },
    #[error("payload nests deeper than {limit}")]
    DepthExceeded { limit: u32 },
    #[error("payload holds more than {limit} values")]
    NodeLimitExceeded { limit: u64 },
    #[error("payload would allocate {requested} bytes, over the {limit}-byte bound")]
    AllocationExceeded { limit: u64, requested: u64 },
    #[error("frame is malformed: {reason}")]
    Malformed { reason: Detail },
    #[error("frame is followed by {extra} unread bytes")]
    TrailingBytes { extra: u64 },
}

/// Encodes and decodes frames under one set of bounds.
#[derive(Clone, Debug)]
pub struct FrameCodec {
    limits: DecodeLimits,
}

impl FrameCodec {
    pub fn new(limits: DecodeLimits) -> Self {
        Self { limits }
    }

    pub fn limits(&self) -> DecodeLimits {
        self.limits
    }

    pub fn encode_parent(&self, frame: &ParentFrame) -> Result<Vec<u8>, CodecRefusal> {
        self.encode(frame)
    }

    pub fn encode_worker(&self, frame: &WorkerFrame) -> Result<Vec<u8>, CodecRefusal> {
        self.encode(frame)
    }

    /// Decodes exactly one parent frame; bytes past it are refused.
    pub fn decode_parent(&self, bytes: &[u8]) -> Result<ParentFrame, CodecRefusal> {
        self.decode_exact(bytes)
    }

    /// Decodes exactly one worker frame; bytes past it are refused.
    pub fn decode_worker(&self, bytes: &[u8]) -> Result<WorkerFrame, CodecRefusal> {
        self.decode_exact(bytes)
    }

    fn encode<T: Serialize>(&self, frame: &T) -> Result<Vec<u8>, CodecRefusal> {
        let limit = u64::from(self.limits.max_frame_bytes);
        // The payload is serialized behind its header's place, so a frame is
        // written once, into one allocation (FIG-4433). The writer's bound is
        // then the whole frame's.
        let mut bytes = Vec::with_capacity(
            INITIAL_FRAME_CAPACITY
                .min(self.limits.max_frame_bytes as usize)
                .max(FRAME_HEADER_BYTES),
        );
        bytes.extend_from_slice(&FRAME_MAGIC);
        bytes.extend_from_slice(&[0; 4]);
        let mut writer = CappedWriter {
            bytes,
            limit: self.limits.max_frame_bytes as usize,
            refused: None,
        };
        let result =
            frame.serialize(&mut rmp_serde::Serializer::new(&mut writer).with_struct_map());
        if let Some(declared) = writer.refused {
            return Err(CodecRefusal::FrameTooLarge { limit, declared });
        }
        result.map_err(|error| CodecRefusal::Malformed {
            reason: Detail::new(format_args!("frame does not encode: {error}")),
        })?;
        if limit < FRAME_HEADER_BYTES as u64 {
            return Err(CodecRefusal::FrameTooLarge {
                limit,
                declared: FRAME_HEADER_BYTES as u64,
            });
        }
        let mut bytes = writer.bytes;
        let payload = (bytes.len() - FRAME_HEADER_BYTES) as u32;
        bytes[FRAME_MAGIC.len()..FRAME_HEADER_BYTES].copy_from_slice(&payload.to_be_bytes());
        Ok(bytes)
    }

    /// The whole frame's length once its header is complete: `None` while
    /// fewer than [`FRAME_HEADER_BYTES`] bytes are present. The header's magic
    /// and declared length are refused as soon as they are readable.
    pub fn frame_len(&self, bytes: &[u8]) -> Result<Option<usize>, CodecRefusal> {
        if bytes.len() < FRAME_HEADER_BYTES {
            if !FRAME_MAGIC.starts_with(&bytes[..bytes.len().min(FRAME_MAGIC.len())]) {
                return Err(CodecRefusal::BadMagic);
            }
            return Ok(None);
        }
        if bytes[..4] != FRAME_MAGIC {
            return Err(CodecRefusal::BadMagic);
        }
        let declared = u32::from_be_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]);
        if u64::from(declared) + FRAME_HEADER_BYTES as u64 > u64::from(self.limits.max_frame_bytes)
        {
            return Err(CodecRefusal::FrameTooLarge {
                limit: u64::from(self.limits.max_frame_bytes),
                declared: u64::from(declared) + FRAME_HEADER_BYTES as u64,
            });
        }
        Ok(Some(FRAME_HEADER_BYTES + declared as usize))
    }

    /// Bounds an embedded MessagePack payload before typed decoding.
    pub fn check_payload(&self, payload: &[u8]) -> Result<(), CodecRefusal> {
        if payload.len() > self.limits.max_frame_bytes as usize {
            return Err(CodecRefusal::FrameTooLarge {
                limit: u64::from(self.limits.max_frame_bytes),
                declared: payload.len() as u64,
            });
        }
        charge_structure(payload, self.limits)
    }

    fn decode_exact<T: DeserializeOwned>(&self, bytes: &[u8]) -> Result<T, CodecRefusal> {
        let Some(total) = self.frame_len(bytes)? else {
            return Err(CodecRefusal::Truncated {
                needed: FRAME_HEADER_BYTES as u64,
                available: bytes.len() as u64,
            });
        };
        if bytes.len() < total {
            return Err(CodecRefusal::Truncated {
                needed: total as u64,
                available: bytes.len() as u64,
            });
        }
        if bytes.len() > total {
            return Err(CodecRefusal::TrailingBytes {
                extra: (bytes.len() - total) as u64,
            });
        }
        let payload = &bytes[FRAME_HEADER_BYTES..total];
        charge_structure(payload, self.limits)?;
        rmp_serde::from_slice(payload).map_err(|error| CodecRefusal::Malformed {
            reason: Detail::new(error),
        })
    }
}

/// Room for a control frame, so the small frames of an effect exchange are
/// encoded without growing.
const INITIAL_FRAME_CAPACITY: usize = 256;

// Stops serialization before extending the allocation beyond the frame cap.
struct CappedWriter {
    bytes: Vec<u8>,
    limit: usize,
    refused: Option<u64>,
}
impl std::io::Write for CappedWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if let Some(size) = &mut self.refused {
            *size = size.saturating_add(bytes.len() as u64);
            return Ok(bytes.len());
        }
        if bytes.len() > self.limit.saturating_sub(self.bytes.len()) {
            self.refused = Some((self.bytes.len() as u64).saturating_add(bytes.len() as u64));
            return Ok(bytes.len());
        }
        let needed = self.bytes.len() + bytes.len();
        if needed > self.bytes.capacity() {
            let capacity = self
                .bytes
                .capacity()
                .saturating_mul(2)
                .max(needed)
                .min(self.limit);
            self.bytes.reserve_exact(capacity - self.bytes.len());
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// The largest MessagePack array header: a marker and a 32-bit count.
const MAX_ARRAY_HEADER_BYTES: u64 = 5;

/// How much a byte string's header grows from an empty one's, at most: an
/// 8-bit length becomes a 32-bit one.
const MAX_BIN_HEADER_GROWTH: u64 = 3;

/// Packs what a step printed into the payloads of
/// [`WorkerMessage::Printed`](crate::WorkerMessage::Printed) frames
/// (FIG-4458).
///
/// Each observation arrives encoded as one MessagePack value, and each
/// payload is an array of whole observations, in order, sized so that its
/// frame is within the codec's bounds and the payload itself passes
/// [`FrameCodec::check_payload`]. So no transport bound limits how many
/// observations a step makes: the run's own budgets do, its fuel how many it
/// can make and its heap how many bytes its worker holds and hands on
/// ([`ObservationChunker::bytes`]). Only an observation that alone outgrows
/// one frame is refused.
#[derive(Debug)]
pub struct ObservationChunker {
    limits: DecodeLimits,
    /// The largest payload one frame carries.
    max_payload_bytes: u64,
    chunks: Vec<crate::EncodedPayload>,
    /// The payload bytes of every closed chunk.
    closed_bytes: u64,
    /// The open chunk's observations, without its array header.
    open: Vec<u8>,
    count: u64,
    charge: Charge,
}

impl FrameCodec {
    /// A chunker whose payloads cross in frames within this codec's bounds.
    pub fn observation_chunker(&self) -> Result<ObservationChunker, CodecRefusal> {
        // The envelope around a payload, under the largest header any lease
        // can carry.
        let envelope = self.encode_worker(&WorkerFrame {
            header: crate::MessageHeader {
                lease: crate::ExecutionLease(u64::MAX),
                owner_epoch: crate::OwnerEpoch(u64::MAX),
                frame_epoch: crate::FrameEpoch(u64::MAX),
                sequence: crate::TransportSequence(u64::MAX),
            },
            message: crate::WorkerMessage::Printed {
                payload: crate::EncodedPayload(Vec::new()),
            },
        })?;
        let envelope_charge = measure_structure(&envelope[FRAME_HEADER_BYTES..], self.limits)?;
        let by_frame = u64::from(self.limits.max_frame_bytes)
            .saturating_sub(envelope.len() as u64 + MAX_BIN_HEADER_GROWTH);
        let by_allocation = self
            .limits
            .max_allocation_bytes
            .saturating_sub(envelope_charge.allocation);
        Ok(ObservationChunker {
            limits: self.limits,
            max_payload_bytes: by_frame.min(by_allocation),
            chunks: Vec::new(),
            closed_bytes: 0,
            open: Vec::new(),
            count: 0,
            charge: Charge::default(),
        })
    }
}

impl ObservationChunker {
    /// Adds one encoded observation after every earlier one. An observation
    /// no frame can carry, even alone, is refused with the bound it
    /// outgrows.
    pub fn push(&mut self, observation: &[u8]) -> Result<(), CodecRefusal> {
        let charge = measure_structure(observation, self.limits)?;
        if !self.admits(
            self.count,
            self.charge,
            self.open.len() as u64,
            charge,
            observation,
        ) {
            self.close();
            if !self.admits(0, Charge::default(), 0, charge, observation) {
                return Err(self.refusal(charge, observation));
            }
        }
        self.open.extend_from_slice(observation);
        self.count += 1;
        self.charge.nodes += charge.nodes;
        self.charge.allocation += charge.allocation;
        self.charge.depth = self.charge.depth.max(charge.depth);
        Ok(())
    }

    /// The payload bytes of every chunk so far, the open one included: what
    /// the step's observations cost the worker that holds them and the
    /// parent that receives them.
    pub fn bytes(&self) -> u64 {
        self.closed_bytes
            + if self.count == 0 {
                0
            } else {
                array_header(self.count).len() as u64 + self.open.len() as u64
            }
    }

    /// The payloads, in order.
    pub fn finish(mut self) -> Vec<crate::EncodedPayload> {
        self.close();
        self.chunks
    }

    /// Whether a chunk of `count` observations, `open_bytes` long and
    /// charged `open`, admits one more.
    fn admits(
        &self,
        count: u64,
        open: Charge,
        open_bytes: u64,
        charge: Charge,
        observation: &[u8],
    ) -> bool {
        let count = count + 1;
        // The array is one more value, and nests its observations one deeper.
        count <= u64::from(u32::MAX)
            && open.nodes + charge.nodes < self.limits.max_nodes
            && (count * CONTAINER_ELEMENT_CHARGE)
                .saturating_add(open.allocation + charge.allocation)
                <= self.limits.max_allocation_bytes
            && open.depth.max(charge.depth) < self.limits.max_depth
            && MAX_ARRAY_HEADER_BYTES + open_bytes + observation.len() as u64
                <= self.max_payload_bytes
    }

    /// The bound an observation that no frame carries outgrows.
    fn refusal(&self, charge: Charge, observation: &[u8]) -> CodecRefusal {
        if charge.nodes >= self.limits.max_nodes {
            CodecRefusal::NodeLimitExceeded {
                limit: self.limits.max_nodes,
            }
        } else if charge.depth >= self.limits.max_depth {
            CodecRefusal::DepthExceeded {
                limit: self.limits.max_depth,
            }
        } else if MAX_ARRAY_HEADER_BYTES + observation.len() as u64 > self.max_payload_bytes {
            CodecRefusal::FrameTooLarge {
                limit: u64::from(self.limits.max_frame_bytes),
                declared: MAX_ARRAY_HEADER_BYTES + observation.len() as u64,
            }
        } else {
            CodecRefusal::AllocationExceeded {
                limit: self.limits.max_allocation_bytes,
                requested: CONTAINER_ELEMENT_CHARGE.saturating_add(charge.allocation),
            }
        }
    }

    fn close(&mut self) {
        if self.count == 0 {
            return;
        }
        let mut payload = array_header(self.count);
        payload.append(&mut self.open);
        self.closed_bytes += payload.len() as u64;
        self.chunks.push(crate::EncodedPayload(payload));
        self.count = 0;
        self.charge = Charge::default();
    }
}

/// The MessagePack header of an array of `count` values.
fn array_header(count: u64) -> Vec<u8> {
    if count < 16 {
        vec![0x90 | count as u8]
    } else if let Ok(count) = u16::try_from(count) {
        let mut header = vec![0xdc];
        header.extend_from_slice(&count.to_be_bytes());
        header
    } else {
        let mut header = vec![0xdd];
        header.extend_from_slice(&(count as u32).to_be_bytes());
        header
    }
}

/// Accumulates a byte stream and yields whole frames.
///
/// The buffer never holds more than one header and one bounded payload: the
/// header's declared length is refused before the payload is waited for. At
/// end of stream, a partial frame is refused rather than decoded; whatever
/// whole frames came before it stand.
#[derive(Debug)]
pub struct FrameReader {
    codec: FrameCodec,
    buffer: Vec<u8>,
}

impl FrameReader {
    pub fn new(codec: FrameCodec) -> Self {
        Self {
            codec,
            buffer: Vec::new(),
        }
    }

    /// Appends received bytes. The header is checked as soon as it is whole.
    pub fn push(&mut self, bytes: &[u8]) -> Result<(), CodecRefusal> {
        self.buffer.extend_from_slice(bytes);
        self.codec.frame_len(&self.buffer).map(|_| ())
    }

    /// The next whole worker frame, or `None` until one has arrived.
    pub fn next_worker(&mut self) -> Result<Option<WorkerFrame>, CodecRefusal> {
        let Some(total) = self.codec.frame_len(&self.buffer)? else {
            return Ok(None);
        };
        if self.buffer.len() < total {
            return Ok(None);
        }
        let rest = self.buffer.split_off(total);
        let frame = std::mem::replace(&mut self.buffer, rest);
        self.codec.decode_worker(&frame).map(Some)
    }

    /// The next whole parent frame, or `None` until one has arrived.
    pub fn next_parent(&mut self) -> Result<Option<ParentFrame>, CodecRefusal> {
        let Some(total) = self.codec.frame_len(&self.buffer)? else {
            return Ok(None);
        };
        if self.buffer.len() < total {
            return Ok(None);
        }
        let rest = self.buffer.split_off(total);
        let frame = std::mem::replace(&mut self.buffer, rest);
        self.codec.decode_parent(&frame).map(Some)
    }

    /// The stream ended: a partial frame still buffered is refused.
    pub fn finish(self) -> Result<(), CodecRefusal> {
        if self.buffer.is_empty() {
            return Ok(());
        }
        let needed = self
            .codec
            .frame_len(&self.buffer)?
            .unwrap_or(FRAME_HEADER_BYTES);
        Err(CodecRefusal::Truncated {
            needed: needed as u64,
            available: self.buffer.len() as u64,
        })
    }
}

/// Walks the payload's MessagePack structure, charging depth, values and
/// declared allocation, without allocating for any of it.
fn charge_structure(payload: &[u8], limits: DecodeLimits) -> Result<(), CodecRefusal> {
    measure_structure(payload, limits).map(|_| ())
}

/// What one payload charged a decode's bounds.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Charge {
    nodes: u64,
    allocation: u64,
    /// The most containers open around any container the payload holds:
    /// wrapped in one more container, the payload nests one deeper.
    depth: u32,
}

/// [`charge_structure`], answering what the payload charged.
fn measure_structure(payload: &[u8], limits: DecodeLimits) -> Result<Charge, CodecRefusal> {
    let mut depth = 0;
    let mut walk = StructureWalk {
        payload,
        position: 0,
        nodes: 0,
        charged: 0,
        limits,
    };
    // One count of values still to read per open container, the root first.
    let mut open: Vec<u64> = Vec::with_capacity(limits.max_depth.min(128) as usize + 1);
    open.push(1);
    while let Some(remaining) = open.last_mut() {
        if *remaining == 0 {
            open.pop();
            continue;
        }
        *remaining -= 1;
        if let Some(children) = walk.value()? {
            if open.len() as u32 > limits.max_depth {
                return Err(CodecRefusal::DepthExceeded {
                    limit: limits.max_depth,
                });
            }
            depth = depth.max(open.len() as u32);
            open.push(children);
        }
    }
    if walk.position != payload.len() {
        return Err(CodecRefusal::TrailingBytes {
            extra: (payload.len() - walk.position) as u64,
        });
    }
    Ok(Charge {
        nodes: walk.nodes,
        allocation: walk.charged,
        depth,
    })
}

struct StructureWalk<'a> {
    payload: &'a [u8],
    position: usize,
    nodes: u64,
    charged: u64,
    limits: DecodeLimits,
}

impl StructureWalk<'_> {
    /// Reads one value's marker and skips its scalar body. A container
    /// answers how many values it holds, for the caller to walk.
    fn value(&mut self) -> Result<Option<u64>, CodecRefusal> {
        self.nodes += 1;
        if self.nodes > self.limits.max_nodes {
            return Err(CodecRefusal::NodeLimitExceeded {
                limit: self.limits.max_nodes,
            });
        }
        let marker = self.take(1)?[0];
        match marker {
            0x00..=0x7f | 0xe0..=0xff | 0xc0 | 0xc2 | 0xc3 => Ok(None),
            0x80..=0x8f => self.container(u64::from(marker & 0x0f) * 2),
            0x90..=0x9f => self.container(u64::from(marker & 0x0f)),
            0xa0..=0xbf => self.blob(u64::from(marker & 0x1f)),
            0xcc | 0xd0 => self.skip(1),
            0xcd | 0xd1 => self.skip(2),
            0xca | 0xce | 0xd2 => self.skip(4),
            0xcb | 0xcf | 0xd3 => self.skip(8),
            0xc4 | 0xd9 => {
                let len = u64::from(self.take(1)?[0]);
                self.blob(len)
            }
            0xc5 | 0xda => {
                let len = self.read_u16()?;
                self.blob(len)
            }
            0xc6 | 0xdb => {
                let len = self.read_u32()?;
                self.blob(len)
            }
            0xdc => {
                let len = self.read_u16()?;
                self.container(len)
            }
            0xdd => {
                let len = self.read_u32()?;
                self.container(len)
            }
            0xde => {
                let len = self.read_u16()?;
                self.container(len * 2)
            }
            0xdf => {
                let len = self.read_u32()?;
                self.container(len * 2)
            }
            0xc1 | 0xc7..=0xc9 | 0xd4..=0xd8 => Err(CodecRefusal::Malformed {
                reason: Detail::new(format_args!(
                    "payload uses marker 0x{marker:02x}, which the protocol never writes"
                )),
            }),
        }
    }

    fn container(&mut self, values: u64) -> Result<Option<u64>, CodecRefusal> {
        // Every value takes at least one byte, so a count past what is left
        // cannot be honest.
        let left = (self.payload.len() - self.position) as u64;
        if values > left {
            return Err(CodecRefusal::Malformed {
                reason: Detail::new(format_args!(
                    "a container declares {values} values with {left} bytes left"
                )),
            });
        }
        self.charge(values * CONTAINER_ELEMENT_CHARGE)?;
        Ok(Some(values))
    }

    fn blob(&mut self, len: u64) -> Result<Option<u64>, CodecRefusal> {
        let left = (self.payload.len() - self.position) as u64;
        if len > left {
            return Err(CodecRefusal::Malformed {
                reason: Detail::new(format_args!(
                    "a string declares {len} bytes with {left} bytes left"
                )),
            });
        }
        self.charge(len)?;
        self.position += len as usize;
        Ok(None)
    }

    fn skip(&mut self, len: usize) -> Result<Option<u64>, CodecRefusal> {
        self.take(len)?;
        Ok(None)
    }

    fn charge(&mut self, bytes: u64) -> Result<(), CodecRefusal> {
        let requested = self.charged.saturating_add(bytes);
        if requested > self.limits.max_allocation_bytes {
            return Err(CodecRefusal::AllocationExceeded {
                limit: self.limits.max_allocation_bytes,
                requested,
            });
        }
        self.charged = requested;
        Ok(())
    }

    fn take(&mut self, len: usize) -> Result<&[u8], CodecRefusal> {
        let end = self
            .position
            .checked_add(len)
            .filter(|end| *end <= self.payload.len());
        let Some(end) = end else {
            return Err(CodecRefusal::Malformed {
                reason: Detail::new("a value runs past the end of the payload"),
            });
        };
        let bytes = &self.payload[self.position..end];
        self.position = end;
        Ok(bytes)
    }

    fn read_u16(&mut self) -> Result<u64, CodecRefusal> {
        let bytes = self.take(2)?;
        Ok(u64::from(u16::from_be_bytes([bytes[0], bytes[1]])))
    }

    fn read_u32(&mut self) -> Result<u64, CodecRefusal> {
        let bytes = self.take(4)?;
        Ok(u64::from(u32::from_be_bytes([
            bytes[0], bytes[1], bytes[2], bytes[3],
        ])))
    }
}

#[cfg(test)]
mod tests;
