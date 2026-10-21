//! One root-based snapshot of live code (`K-CLO-004`).
//!
//! A saved function and a function crossing to a tool are both cut out of
//! a live heap from one root: the closure's code, the helpers it reads,
//! the declared functions it names, and the captures it reads frozen. A
//! saved function freezes its captures as pages a later cell instantiates
//! ([`SnapshotHeap::Frozen`]); a crossing function freezes them as
//! constants in a self-contained document ([`extract`]). Both first find
//! every closure the root reaches through data ([`reach`]), so a capture
//! holding one refers to it by name.

mod collect;
mod reach;
mod reads;

pub use collect::{
    CaptureCheck, CaptureRefusal, FrozenToken, Gathered, SnapshotError, SnapshotHeap,
    SnapshotRefusal, closure_at, closure_held, freeze_token, gather, members, text_is, text_of,
    token_closure, validate_capture,
};
pub use reach::{Found, reach};
pub use reads::CodeReads;
