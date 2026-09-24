//! The Restate service-protocol codec: the framing the runtime and an SDK
//! exchange over one invocation stream, and the protobuf messages they frame.
//!
//! `restate-sdk-shared-core` keeps its codec `pub(crate)`, so the double
//! reimplements it from the public protocol: the messages are the vendored
//! `protocol.proto` compiled by prost ([`generated`]), and the framing is the
//! 64-bit header the protocol document specifies (`type << 48 | flags | len`,
//! with the `REQUESTED_ACK` flag at bit 47).

mod frame;
#[allow(
    clippy::all,
    missing_docs,
    reason = "prost-build output for the vendored Restate service protocol"
)]
pub mod generated;

pub use frame::{Frame, FrameDecoder, FrameError, MessageType, encode_frame, encode_message};

/// The protocol versions the double speaks. Restate 1.7 negotiates V6 unless
/// the server opts into V7, so V6 is what production runs and the default.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ProtocolVersion {
    V6,
    V7,
}

impl ProtocolVersion {
    pub const fn content_type(self) -> &'static str {
        match self {
            Self::V6 => "application/vnd.restate.invocation.v6",
            Self::V7 => "application/vnd.restate.invocation.v7",
        }
    }
}

/// The pre-V7 suspension message: the awaited notification ids as three flat
/// lists. V7 replaced it with a [`generated::Future`] tree on field 4 and
/// reserved these fields, so the generated message cannot decode it.
#[derive(Clone, PartialEq, prost::Message)]
pub struct SuspensionMessageV6 {
    #[prost(uint32, repeated, tag = "1")]
    pub waiting_completions: Vec<u32>,
    #[prost(uint32, repeated, tag = "2")]
    pub waiting_signals: Vec<u32>,
    #[prost(string, repeated, tag = "3")]
    pub waiting_named_signals: Vec<String>,
}

/// The built-in signal index Restate cancels an invocation through.
pub const CANCEL_SIGNAL_ID: u32 = 1;
