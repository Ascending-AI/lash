//! Materialization maps: how a compiled exit rebuilds the canonical frame.
//!
//! At every saveable exit, compiled code must leave exactly the frame the
//! interpreter holds at that Boundary, so the parked-run capture and
//! restore stay the only serializer and deserializer and no new state is
//! persisted. A [`FrameMap`] says where each part of that frame is when it
//! is not already canonical. Read the other way, it is the entry map of a
//! continuation label.
//!
//! Maps are part of an artifact image: compact tables sorted by Boundary,
//! LEB128-encoded, versioned by [`MAP_ENCODING_VERSION`] (which the ABI
//! digest covers), and free of addresses.

use crate::kir::{BoundaryIx, ConstIx, KirFunction, LoopIx, Reg, SlotAt, TryIx};

/// The version of the map encoding.
pub const MAP_ENCODING_VERSION: u32 = 1;

/// One per compiled function and Boundary or action site.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct FrameMap {
    /// Gives the site, the static control recipe and `awaiting`.
    pub boundary: BoundaryIx,
    /// Only slots not already canonical; a baseline map has none.
    pub slots: Box<[(SlotAt, Loc)]>,
    /// Dynamic loop state: the cursor's position and `started`.
    pub loops: Box<[(LoopIx, LoopLoc)]>,
    /// The pending departure of each active `finally`.
    pub finally: Box<[(TryIx, DepartLoc)]>,
    /// Units owed for the batched region up to here.
    pub charge_prefix: u64,
    /// Bytes owed for batched reservations up to here.
    pub memory_prefix: u64,
}

impl FrameMap {
    /// The map of a Boundary where the whole frame is canonical: every
    /// baseline exit's map.
    pub fn baseline(boundary: BoundaryIx) -> Self {
        Self {
            boundary,
            slots: Box::new([]),
            loops: Box::new([]),
            finally: Box::new([]),
            charge_prefix: 0,
            memory_prefix: 0,
        }
    }
}

/// One baseline map per Boundary of `f`, in Boundary order.
pub fn baseline_maps(f: &KirFunction) -> Vec<FrameMap> {
    (0..f.boundaries.len() as u32)
        .map(|b| FrameMap::baseline(BoundaryIx(b)))
        .collect()
}

/// Where a value is at an exit.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Loc {
    /// Already where the canonical frame keeps it.
    Canonical,
    Reg(Reg),
    /// A slot of the exit's spill area.
    Spill(u32),
    Const(ConstIx),
    /// The slot is empty.
    Empty,
}

/// Where a loop's dynamic state is at an exit.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum LoopLoc {
    Canonical,
    Regs { position: Loc, started: Loc },
}

/// Where a `finally`'s pending departure is at an exit.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum DepartLoc {
    Canonical,
    Normal,
    Break(LoopIx),
    Continue(LoopIx),
    Return(Loc),
    Throw(Loc),
}

/// Why a map table does not decode.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum MapDecodeError {
    #[error("the map table states encoding version {0}")]
    Version(u64),
    #[error("the map table ends inside an entry")]
    Truncated,
    #[error("the map table holds a number that does not fit or an unknown tag")]
    Malformed,
    #[error("the maps are not in strictly increasing Boundary order")]
    Unsorted,
    #[error("bytes follow the last map")]
    Trailing,
}

/// Encodes a function's maps, sorted by Boundary.
pub fn encode_maps(maps: &[FrameMap]) -> Vec<u8> {
    let mut out = Vec::new();
    leb(u64::from(MAP_ENCODING_VERSION), &mut out);
    leb(maps.len() as u64, &mut out);
    for map in maps {
        leb(u64::from(map.boundary.0), &mut out);
        leb(map.slots.len() as u64, &mut out);
        for (slot, loc) in &map.slots {
            leb(u64::from(slot.0), &mut out);
            encode_loc(*loc, &mut out);
        }
        leb(map.loops.len() as u64, &mut out);
        for (lp, loc) in &map.loops {
            leb(u64::from(lp.0), &mut out);
            match loc {
                LoopLoc::Canonical => out.push(0),
                LoopLoc::Regs { position, started } => {
                    out.push(1);
                    encode_loc(*position, &mut out);
                    encode_loc(*started, &mut out);
                }
            }
        }
        leb(map.finally.len() as u64, &mut out);
        for (t, loc) in &map.finally {
            leb(u64::from(t.0), &mut out);
            match loc {
                DepartLoc::Canonical => out.push(0),
                DepartLoc::Normal => out.push(1),
                DepartLoc::Break(lp) => {
                    out.push(2);
                    leb(u64::from(lp.0), &mut out);
                }
                DepartLoc::Continue(lp) => {
                    out.push(3);
                    leb(u64::from(lp.0), &mut out);
                }
                DepartLoc::Return(value) => {
                    out.push(4);
                    encode_loc(*value, &mut out);
                }
                DepartLoc::Throw(value) => {
                    out.push(5);
                    encode_loc(*value, &mut out);
                }
            }
        }
        leb(map.charge_prefix, &mut out);
        leb(map.memory_prefix, &mut out);
    }
    out
}

/// Decodes what [`encode_maps`] encodes.
pub fn decode_maps(bytes: &[u8]) -> Result<Vec<FrameMap>, MapDecodeError> {
    let mut r = Reader { bytes, at: 0 };
    let version = r.u64()?;
    if version != u64::from(MAP_ENCODING_VERSION) {
        return Err(MapDecodeError::Version(version));
    }
    let count = r.len()?;
    let mut maps: Vec<FrameMap> = Vec::new();
    for _ in 0..count {
        let boundary = BoundaryIx(r.u32()?);
        if maps.last().is_some_and(|last| last.boundary >= boundary) {
            return Err(MapDecodeError::Unsorted);
        }
        let slots = (0..r.len()?)
            .map(|_| Ok((SlotAt(r.u32()?), r.loc()?)))
            .collect::<Result<_, MapDecodeError>>()?;
        let loops = (0..r.len()?)
            .map(|_| {
                let lp = LoopIx(r.u32()?);
                let loc = match r.byte()? {
                    0 => LoopLoc::Canonical,
                    1 => LoopLoc::Regs {
                        position: r.loc()?,
                        started: r.loc()?,
                    },
                    _ => return Err(MapDecodeError::Malformed),
                };
                Ok((lp, loc))
            })
            .collect::<Result<_, MapDecodeError>>()?;
        let finally = (0..r.len()?)
            .map(|_| {
                let t = TryIx(r.u32()?);
                let loc = match r.byte()? {
                    0 => DepartLoc::Canonical,
                    1 => DepartLoc::Normal,
                    2 => DepartLoc::Break(LoopIx(r.u32()?)),
                    3 => DepartLoc::Continue(LoopIx(r.u32()?)),
                    4 => DepartLoc::Return(r.loc()?),
                    5 => DepartLoc::Throw(r.loc()?),
                    _ => return Err(MapDecodeError::Malformed),
                };
                Ok((t, loc))
            })
            .collect::<Result<_, MapDecodeError>>()?;
        maps.push(FrameMap {
            boundary,
            slots,
            loops,
            finally,
            charge_prefix: r.u64()?,
            memory_prefix: r.u64()?,
        });
    }
    if r.at != bytes.len() {
        return Err(MapDecodeError::Trailing);
    }
    Ok(maps)
}

fn leb(mut n: u64, out: &mut Vec<u8>) {
    loop {
        let low = (n & 0x7f) as u8;
        n >>= 7;
        if n == 0 {
            out.push(low);
            return;
        }
        out.push(low | 0x80);
    }
}

fn encode_loc(loc: Loc, out: &mut Vec<u8>) {
    match loc {
        Loc::Canonical => out.push(0),
        Loc::Reg(reg) => {
            out.push(1);
            leb(u64::from(reg.0), out);
        }
        Loc::Spill(at) => {
            out.push(2);
            leb(u64::from(at), out);
        }
        Loc::Const(c) => {
            out.push(3);
            leb(u64::from(c.0), out);
        }
        Loc::Empty => out.push(4),
    }
}

struct Reader<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl Reader<'_> {
    fn byte(&mut self) -> Result<u8, MapDecodeError> {
        let byte = *self.bytes.get(self.at).ok_or(MapDecodeError::Truncated)?;
        self.at += 1;
        Ok(byte)
    }

    fn u64(&mut self) -> Result<u64, MapDecodeError> {
        let mut n = 0u64;
        for shift in (0..64).step_by(7) {
            let byte = self.byte()?;
            let bits = u64::from(byte & 0x7f);
            if shift == 63 && bits > 1 {
                return Err(MapDecodeError::Malformed);
            }
            n |= bits << shift;
            if byte & 0x80 == 0 {
                return Ok(n);
            }
        }
        Err(MapDecodeError::Malformed)
    }

    fn u32(&mut self) -> Result<u32, MapDecodeError> {
        u32::try_from(self.u64()?).map_err(|_| MapDecodeError::Malformed)
    }

    /// A count of entries, each at least one byte long.
    fn len(&mut self) -> Result<usize, MapDecodeError> {
        let len = usize::try_from(self.u64()?).map_err(|_| MapDecodeError::Malformed)?;
        if len > self.bytes.len() - self.at {
            return Err(MapDecodeError::Truncated);
        }
        Ok(len)
    }

    fn loc(&mut self) -> Result<Loc, MapDecodeError> {
        Ok(match self.byte()? {
            0 => Loc::Canonical,
            1 => Loc::Reg(Reg(self.u32()?)),
            2 => Loc::Spill(self.u32()?),
            3 => Loc::Const(ConstIx(self.u32()?)),
            4 => Loc::Empty,
            _ => return Err(MapDecodeError::Malformed),
        })
    }
}
