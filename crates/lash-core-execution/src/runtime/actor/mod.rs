//! [`ActorContext`]: the one effect context of the durable substrate
//! (ADR 0132 §1, §3; I0, FIG-5194).
//!
//! An actor's activation runs everything it does through one concrete
//! context: no engine trait, no controller trait, no default bodies. The
//! context's core ([`ActorContext::begin`], [`ActorContext::commit`], the
//! clock, the cancel token, the replay probe and the due times) is real.
//! What each runtime lane builds on it lives in that lane's file, as
//! [`ActorContext`] methods and free functions whose bodies are tagged
//! `todo!()`s until the lane fills them:
//!
//! | File | Owner |
//! |---|---|
//! | `core.rs` | I0 (real) |
//! | [`turn`] | L3 |
//! | [`round`] | V0, then L4 |
//! | [`ingress`] | L3s |
//! | [`waits`], `wait_effects.rs` | L5 |
//! | `await_event_legacy.rs` | L3, L4, L6 (deleted with their ports) |
//! | [`process`] | L6 |
//! | [`vm`] | V0, then L7 |
//! | [`projection`] | L7p |

mod await_event_legacy;
pub use await_event_legacy::completion_host_key;
mod core;
pub mod ingress;
pub mod journal;
pub mod process;
#[cfg(feature = "testing")]
pub mod process_laws;
pub mod projection;
pub mod round;
pub mod turn;
pub mod vm;
mod wait_effects;
#[cfg(feature = "testing")]
pub mod wait_laws;
pub mod waits;

pub use core::ActorContext;
