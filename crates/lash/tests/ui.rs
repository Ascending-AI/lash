//! Compile-time contracts for the `lash` facade surface.
//!
//! Two kinds of contract live here and they are enforced by two different
//! mechanisms.
//!
//! *Compile-fail* contracts are the `.stderr` fixtures: each one must fail
//! to compile with exactly the diagnostic it pins, under the direct-rustc
//! `ui_fixtures` gate.
//!
//! *Compile-pass* contracts are the modules below. They assert the opposite --
//! that a published surface is reachable and spellable from outside the crate
//! -- and a `mod` of this test binary proves that: if any of them stops
//! compiling, this binary stops building and the suite fails.
// The fixtures are written as standalone programs: each keeps a `fn main`
// and imports the whole surface it is asserting on, and the surface probes
// unwrap on values they construct themselves. As modules they inherit this
// binary's lint scope, so the allowances they relied on are spelled here,
// scoped to this binary only.
#![allow(dead_code, unused_imports)]
#![allow(clippy::unwrap_used, clippy::expect_used)]

// The compile-pass surface fixtures. `#[path]` keeps the files where the
// compile-fail fixtures live, so the directory stays the single home of the
// fixture set.
#[cfg(feature = "rlm")]
#[path = "ui/durable_builder_without_advanced.rs"]
mod durable_builder_without_advanced;
#[path = "ui/facade_boundary_types_are_public.rs"]
mod facade_boundary_types_are_public;
#[path = "ui/observation_enums_match_without_mcp.rs"]
mod observation_enums_match_without_mcp;
#[path = "ui/process_surface_types_are_public.rs"]
mod process_surface_types_are_public;
#[cfg(feature = "rlm")]
#[path = "ui/rlm_facade_boundary_types_are_public.rs"]
mod rlm_facade_boundary_types_are_public;
