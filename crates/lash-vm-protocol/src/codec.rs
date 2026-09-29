//! Length-framed encoding under an exact build identity, and bounded decoding.
//!
//! A frame is a fixed header and a MessagePack payload:
//!
//! ```text
//! magic "LVMP" (4) | build digest (32) | payload length, u32 big-endian (4) | payload
//! ```
//!
//! Decoding refuses, with a typed [`CodecRefusal`] and before allocating for
//! the payload:
//!
//! - a frame of another build (its digest is not this build's);
//! - a declared payload over [`DecodeLimits::max_frame_bytes`];
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

use serde::Serialize;
use serde::de::DeserializeOwned;
use thiserror::Error;

use crate::identity::BuildIdentity;
use crate::message::{ParentFrame, WorkerFrame};

pub const FRAME_MAGIC: [u8; 4] = *b"LVMP";

/// Magic, build digest and payload length.
pub const FRAME_HEADER_BYTES: usize = 4 + 32 + 4;

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

#[derive(Clone, Debug, PartialEq, Eq, Error)]
pub enum CodecRefusal {
    #[error("frame is truncated: {needed} bytes needed, {available} available")]
    Truncated { needed: u64, available: u64 },
    #[error("frame does not start with the protocol magic")]
    BadMagic,
    #[error("frame was written by build {found}, expected {expected}")]
    WrongBuild { expected: String, found: String },
    #[error("frame declares a {declared}-byte payload, over the {limit}-byte bound")]
    FrameTooLarge { limit: u64, declared: u64 },
    #[error("payload nests deeper than {limit}")]
    DepthExceeded { limit: u32 },
    #[error("payload holds more than {limit} values")]
    NodeLimitExceeded { limit: u64 },
    #[error("payload would allocate {requested} bytes, over the {limit}-byte bound")]
    AllocationExceeded { limit: u64, requested: u64 },
    #[error("frame is malformed: {reason}")]
    Malformed { reason: String },
    #[error("frame is followed by {extra} unread bytes")]
    TrailingBytes { extra: u64 },
}

/// Encodes and decodes frames for one build under one set of bounds.
#[derive(Clone, Debug)]
pub struct FrameCodec {
    build: BuildIdentity,
    digest: [u8; 32],
    limits: DecodeLimits,
}

impl FrameCodec {
    pub fn new(build: BuildIdentity, limits: DecodeLimits) -> Self {
        let digest = build.digest();
        Self {
            build,
            digest,
            limits,
        }
    }

    pub fn build(&self) -> &BuildIdentity {
        &self.build
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
        let payload = rmp_serde::to_vec_named(frame).map_err(|error| CodecRefusal::Malformed {
            reason: format!("frame does not encode: {error}"),
        })?;
        let declared = payload.len() as u64;
        if declared > u64::from(self.limits.max_frame_bytes) {
            return Err(CodecRefusal::FrameTooLarge {
                limit: u64::from(self.limits.max_frame_bytes),
                declared,
            });
        }
        let mut bytes = Vec::with_capacity(FRAME_HEADER_BYTES + payload.len());
        bytes.extend_from_slice(&FRAME_MAGIC);
        bytes.extend_from_slice(&self.digest);
        bytes.extend_from_slice(&(payload.len() as u32).to_be_bytes());
        bytes.extend_from_slice(&payload);
        Ok(bytes)
    }

    /// The whole frame's length once its header is complete: `None` while
    /// fewer than [`FRAME_HEADER_BYTES`] bytes are present. The header's magic,
    /// build and declared length are refused as soon as they are readable.
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
        if bytes[4..36] != self.digest {
            return Err(CodecRefusal::WrongBuild {
                expected: hex(&self.digest),
                found: hex(&bytes[4..36]),
            });
        }
        let declared = u32::from_be_bytes([bytes[36], bytes[37], bytes[38], bytes[39]]);
        if declared > self.limits.max_frame_bytes {
            return Err(CodecRefusal::FrameTooLarge {
                limit: u64::from(self.limits.max_frame_bytes),
                declared: u64::from(declared),
            });
        }
        Ok(Some(FRAME_HEADER_BYTES + declared as usize))
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
            reason: error.to_string(),
        })
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
            open.push(children);
        }
    }
    if walk.position != payload.len() {
        return Err(CodecRefusal::TrailingBytes {
            extra: (payload.len() - walk.position) as u64,
        });
    }
    Ok(())
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
                reason: format!(
                    "payload uses marker 0x{marker:02x}, which the protocol never writes"
                ),
            }),
        }
    }

    fn container(&mut self, values: u64) -> Result<Option<u64>, CodecRefusal> {
        // Every value takes at least one byte, so a count past what is left
        // cannot be honest.
        let left = (self.payload.len() - self.position) as u64;
        if values > left {
            return Err(CodecRefusal::Malformed {
                reason: format!("a container declares {values} values with {left} bytes left"),
            });
        }
        self.charge(values * CONTAINER_ELEMENT_CHARGE)?;
        Ok(Some(values))
    }

    fn blob(&mut self, len: u64) -> Result<Option<u64>, CodecRefusal> {
        let left = (self.payload.len() - self.position) as u64;
        if len > left {
            return Err(CodecRefusal::Malformed {
                reason: format!("a string declares {len} bytes with {left} bytes left"),
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
                reason: "a value runs past the end of the payload".to_string(),
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

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[cfg(test)]
mod tests;
