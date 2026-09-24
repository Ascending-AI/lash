//! Message framing: the 8-byte header in front of every protobuf payload.
//!
//! ```text
//!  0               16 17              32                              64
//! +-----------------+-+---------------+--------------------------------+
//! |      type       |A|   reserved    |             length             |
//! +-----------------+-+---------------+--------------------------------+
//! ```
//!
//! `A` is `REQUESTED_ACK`, meaningful only on `ProposeRunCompletion`.

use bytes::{Buf, BufMut, Bytes, BytesMut};

const HEADER_LEN: usize = 8;
const REQUESTED_ACK_MASK: u64 = 0x8000_0000_0000;
const LENGTH_MASK: u64 = 0xFFFF_FFFF;
const COMMAND_MASK: u16 = 0x0400;
const NOTIFICATION_MASK: u16 = 0x8000;
const CUSTOM_MASK: u16 = 0xFC00;

macro_rules! message_types {
    ($($variant:ident = $code:literal,)*) => {
        /// Every message type of the service protocol, by its 16-bit code.
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
        pub enum MessageType {
            $($variant,)*
            /// A custom command (`0xFC00..=0xFFFF`): opaque, journaled and
            /// replayed verbatim.
            Custom(u16),
        }

        impl MessageType {
            pub const fn code(self) -> u16 {
                match self {
                    $(Self::$variant => $code,)*
                    Self::Custom(code) => code,
                }
            }

            pub fn from_code(code: u16) -> Result<Self, FrameError> {
                match code {
                    $($code => Ok(Self::$variant),)*
                    code if code & CUSTOM_MASK == CUSTOM_MASK => Ok(Self::Custom(code)),
                    code => Err(FrameError::UnknownType(code)),
                }
            }
        }
    };
}

message_types!(
    Start = 0x0000,
    Suspension = 0x0001,
    Error = 0x0002,
    End = 0x0003,
    CommandAck = 0x0004,
    ProposeRunCompletion = 0x0005,
    AwaitingOn = 0x0006,
    ProposeRunCompletionAck = 0x0007,
    InputCommand = 0x0400,
    OutputCommand = 0x0401,
    GetLazyStateCommand = 0x0402,
    SetStateCommand = 0x0403,
    ClearStateCommand = 0x0404,
    ClearAllStateCommand = 0x0405,
    GetLazyStateKeysCommand = 0x0406,
    GetEagerStateCommand = 0x0407,
    GetEagerStateKeysCommand = 0x0408,
    GetPromiseCommand = 0x0409,
    PeekPromiseCommand = 0x040A,
    CompletePromiseCommand = 0x040B,
    SleepCommand = 0x040C,
    CallCommand = 0x040D,
    OneWayCallCommand = 0x040E,
    SendSignalCommand = 0x0410,
    RunCommand = 0x0411,
    AttachInvocationCommand = 0x0412,
    GetInvocationOutputCommand = 0x0413,
    CompleteAwakeableCommand = 0x0414,
    GetLazyStateCompletionNotification = 0x8002,
    GetLazyStateKeysCompletionNotification = 0x8006,
    GetPromiseCompletionNotification = 0x8009,
    PeekPromiseCompletionNotification = 0x800A,
    CompletePromiseCompletionNotification = 0x800B,
    SleepCompletionNotification = 0x800C,
    CallCompletionNotification = 0x800D,
    CallInvocationIdCompletionNotification = 0x800E,
    RunCompletionNotification = 0x8011,
    AttachInvocationCompletionNotification = 0x8012,
    GetInvocationOutputCompletionNotification = 0x8013,
    SignalNotification = 0xFBFF,
);

impl MessageType {
    /// A journal command: written by the SDK, replayed to it in order.
    pub fn is_command(self) -> bool {
        (COMMAND_MASK..NOTIFICATION_MASK).contains(&self.code()) || matches!(self, Self::Custom(_))
    }

    /// A journal notification: written by the runtime, replayed in the order
    /// the SDK first observed it.
    pub fn is_notification(self) -> bool {
        (NOTIFICATION_MASK..CUSTOM_MASK).contains(&self.code())
    }

    fn allows_ack(self) -> bool {
        matches!(self, Self::ProposeRunCompletion)
    }
}

/// Why a byte stream is not a well-formed protocol stream.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum FrameError {
    #[error("unknown protocol message type {0:#06x}")]
    UnknownType(u16),
    #[error("protocol message {ty:?} does not decode: {detail}")]
    Decode { ty: MessageType, detail: String },
    #[error("the protocol stream ended inside a frame ({buffered} bytes buffered)")]
    Truncated { buffered: usize },
}

/// One framed protocol message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Frame {
    pub ty: MessageType,
    /// The `REQUESTED_ACK` flag; always `false` on types that do not carry it.
    pub requested_ack: bool,
    pub payload: Bytes,
}

impl Frame {
    pub fn new(ty: MessageType, payload: Bytes) -> Self {
        Self {
            ty,
            requested_ack: false,
            payload,
        }
    }

    pub fn of<M: prost::Message>(ty: MessageType, message: &M) -> Self {
        Self::new(ty, Bytes::from(message.encode_to_vec()))
    }

    /// Decode the payload as `M`.
    pub fn decode<M: prost::Message + Default>(&self) -> Result<M, FrameError> {
        M::decode(self.payload.clone()).map_err(|error| FrameError::Decode {
            ty: self.ty,
            detail: error.to_string(),
        })
    }

    /// The frame as it goes on the wire: header then payload.
    pub fn encode(&self) -> Bytes {
        encode_frame(self.ty, self.requested_ack, &self.payload)
    }
}

/// Encode one frame: the header, then `payload`.
pub fn encode_frame(ty: MessageType, requested_ack: bool, payload: &[u8]) -> Bytes {
    let mut header = (u64::from(ty.code()) << 48) | (payload.len() as u64 & LENGTH_MASK);
    if requested_ack && ty.allows_ack() {
        header |= REQUESTED_ACK_MASK;
    }
    let mut buffer = BytesMut::with_capacity(HEADER_LEN + payload.len());
    buffer.put_u64(header);
    buffer.extend_from_slice(payload);
    buffer.freeze()
}

/// Encode one protobuf message as a frame of type `ty`.
pub fn encode_message<M: prost::Message>(ty: MessageType, message: &M) -> Bytes {
    encode_frame(ty, false, &message.encode_to_vec())
}

/// Incremental frame decoder: push stream chunks, pop whole frames.
#[derive(Debug, Default)]
pub struct FrameDecoder {
    buffer: BytesMut,
}

impl FrameDecoder {
    pub fn push(&mut self, chunk: &[u8]) {
        self.buffer.extend_from_slice(chunk);
    }

    /// The next whole frame, or `None` until more bytes arrive.
    pub fn next_frame(&mut self) -> Result<Option<Frame>, FrameError> {
        if self.buffer.len() < HEADER_LEN {
            return Ok(None);
        }
        let mut header_bytes = &self.buffer[..HEADER_LEN];
        let header = header_bytes.get_u64();
        let code = (header >> 48) as u16;
        let ty = MessageType::from_code(code)?;
        let length = usize::try_from(header & LENGTH_MASK).unwrap_or(usize::MAX);
        let Some(frame_len) = HEADER_LEN.checked_add(length) else {
            return Err(FrameError::Truncated {
                buffered: self.buffer.len(),
            });
        };
        if self.buffer.len() < frame_len {
            return Ok(None);
        }
        let mut frame = self.buffer.split_to(frame_len);
        frame.advance(HEADER_LEN);
        Ok(Some(Frame {
            ty,
            requested_ack: ty.allows_ack() && header & REQUESTED_ACK_MASK != 0,
            payload: frame.freeze(),
        }))
    }

    /// Every whole frame currently buffered.
    pub fn drain(&mut self) -> Result<Vec<Frame>, FrameError> {
        let mut frames = Vec::new();
        while let Some(frame) = self.next_frame()? {
            frames.push(frame);
        }
        Ok(frames)
    }

    /// Fail if the stream ended with a partial frame buffered.
    pub fn finish(&self) -> Result<(), FrameError> {
        if self.buffer.is_empty() {
            Ok(())
        } else {
            Err(FrameError::Truncated {
                buffered: self.buffer.len(),
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::generated::{ProposeRunCompletionMessage, StartMessage};
    use prost::Message as _;

    #[test]
    fn header_carries_type_length_and_the_ack_flag_only_where_allowed() {
        let encoded = encode_frame(MessageType::ProposeRunCompletion, true, b"abc");
        assert_eq!(
            u64::from_be_bytes(encoded[..8].try_into().unwrap()),
            (0x0005_u64 << 48) | REQUESTED_ACK_MASK | 3
        );
        let unflagged = encode_frame(MessageType::RunCommand, true, b"abc");
        assert_eq!(
            u64::from_be_bytes(unflagged[..8].try_into().unwrap()),
            (0x0411_u64 << 48) | 3
        );
    }

    #[test]
    fn decoder_reassembles_frames_split_across_chunks() {
        let start = StartMessage {
            debug_id: "inv_1".into(),
            known_entries: 1,
            ..Default::default()
        };
        let propose = ProposeRunCompletionMessage {
            result_completion_id: 7,
            result: None,
        };
        let mut stream = BytesMut::new();
        stream.extend_from_slice(&encode_message(MessageType::Start, &start));
        stream.extend_from_slice(&encode_frame(
            MessageType::ProposeRunCompletion,
            true,
            &propose.encode_to_vec(),
        ));
        let mut decoder = FrameDecoder::default();
        let mut frames = Vec::new();
        for chunk in stream.chunks(3) {
            decoder.push(chunk);
            frames.extend(decoder.drain().unwrap());
        }
        decoder.finish().unwrap();
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[0].decode::<StartMessage>().unwrap(), start);
        assert!(frames[1].requested_ack);
        assert_eq!(
            frames[1].decode::<ProposeRunCompletionMessage>().unwrap(),
            propose
        );
    }

    #[test]
    fn custom_and_unknown_codes_are_told_apart() {
        assert_eq!(
            MessageType::from_code(0xFC01).unwrap(),
            MessageType::Custom(0xFC01)
        );
        assert!(MessageType::Custom(0xFC01).is_command());
        assert_eq!(
            MessageType::from_code(0x0099),
            Err(FrameError::UnknownType(0x0099))
        );
    }
}
