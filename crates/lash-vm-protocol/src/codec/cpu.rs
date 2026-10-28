//! Keep CPU receipts unsigned integers, with a constant MessagePack width.

use std::io::{self, Write};

use crate::{BorrowedParentFrame, ParentFrame, PoolStarted, WorkerFrame, WorkerMessage};

pub(super) trait FrameEncoding: serde::Serialize {
    fn cpu_counter(&self) -> Option<u64> {
        None
    }
}

impl FrameEncoding for ParentFrame {}
impl FrameEncoding for BorrowedParentFrame<'_> {}
impl FrameEncoding for PoolStarted {}
impl FrameEncoding for WorkerFrame {
    fn cpu_counter(&self) -> Option<u64> {
        match &self.message {
            WorkerMessage::Progress { cpu_nanos, .. } | WorkerMessage::ResetDone { cpu_nanos } => {
                Some(*cpu_nanos)
            }
            _ => None,
        }
    }
}

enum State {
    Name,
    Integer,
    Skip(usize),
    Done,
}

pub(super) struct Writer<'a, W> {
    inner: &'a mut W,
    counter: u64,
    state: State,
}

impl<'a, W> Writer<'a, W> {
    pub(super) fn new(inner: &'a mut W, counter: Option<u64>) -> Self {
        Self {
            inner,
            counter: counter.unwrap_or_default(),
            state: if counter.is_some() {
                State::Name
            } else {
                State::Done
            },
        }
    }

    pub(super) fn complete(&self) -> bool {
        matches!(self.state, State::Done)
    }
}

impl<W: Write> Write for Writer<'_, W> {
    fn write(&mut self, mut bytes: &[u8]) -> io::Result<usize> {
        let supplied = bytes.len();
        while !bytes.is_empty() {
            match self.state {
                // rmp-serde writes each named field as one string body. Only
                // Progress/ResetDone arm this writer: their numeric/UUID header
                // and fixed phase precede CPU, so no user text can match here.
                State::Name if bytes == b"cpu_nanos" => {
                    self.inner.write_all(bytes)?;
                    self.state = State::Integer;
                    break;
                }
                State::Integer => {
                    let encoded = match bytes[0] {
                        0x00..=0x7f => 1,
                        0xcc => 2,
                        0xcd => 3,
                        0xce => 5,
                        0xcf => 9,
                        _ => {
                            return Err(io::Error::other("CPU receipt is not an unsigned integer"));
                        }
                    };
                    let mut fixed = [0; 9];
                    fixed[0] = 0xcf;
                    fixed[1..].copy_from_slice(&self.counter.to_be_bytes());
                    self.inner.write_all(&fixed)?;
                    self.state = State::Skip(encoded);
                }
                State::Skip(remaining) => {
                    let consumed = remaining.min(bytes.len());
                    bytes = &bytes[consumed..];
                    self.state = if consumed == remaining {
                        State::Done
                    } else {
                        State::Skip(remaining - consumed)
                    };
                }
                _ => {
                    self.inner.write_all(bytes)?;
                    break;
                }
            }
        }
        Ok(supplied)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}
