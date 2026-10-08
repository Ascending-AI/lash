//! [`ActorContext`]: the one effect context of the durable substrate
//! (ADR 0132 §1, §3; I0, FIG-5194).
//!
//! An actor's activation runs everything it does through one concrete
//! context: no engine trait, no controller trait, no default bodies. The
//! context's core ([`ActorContext::begin`], [`ActorContext::commit`], the
//! clock, the cancel token, the replay probe and the due times) is real.

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
