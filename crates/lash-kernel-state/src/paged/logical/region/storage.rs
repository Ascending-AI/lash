//! Frozen nominal lease value; lane D owns its representation and constructors.
#![allow(dead_code, reason = "declaration-only handoff to lane D")]

/// An inline scalar or a lease-qualified identity. No canonical handle alias.
/// The declaration cannot construct a production value before D implements it.
#[derive(Clone, Copy)]
pub struct LeaseValue {
    sealed: std::convert::Infallible,
}
