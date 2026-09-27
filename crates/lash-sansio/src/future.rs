//! The erased future shape the drive's journaled boundaries share.
//!
//! `Pin<Box<dyn Future<Output = T> + Send + 'a>>` is the spelling every
//! journaled seam already uses: a recorded step's body handed to the engine,
//! a host ability call's reply through the run's command protocol. The
//! drive-determinism lint pins the tokens wherever they appear spelled out,
//! so each boundary names the shape through here — a crate outside the
//! scanned drive paths — rather than repeating them (FIG-3672).
//!
//! There is deliberately no synchronous driver here: code that wants a
//! future's answer inside the scanned drive paths must be restructured so the
//! awaited value is already available, the way FIG-3903 made projected
//! descriptor reads synchronous and pure — a hand-rolled `block_on` lets live
//! async scheduling leak into decisions the drive requires to be
//! deterministic.

/// A `Send`-boxed future, lifetime-bound to its call site.
pub type SendBoxFuture<'a, T> = std::pin::Pin<Box<dyn std::future::Future<Output = T> + Send + 'a>>;
