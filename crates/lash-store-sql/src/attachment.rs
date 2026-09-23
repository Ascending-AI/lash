//! The attachment family: the write-ahead manifest, the GC condemnation
//! fence, and SQLite's attachment bytes.
//!
//! Two tables — [`manifest`] and [`condemnation`] — and one rule that spans
//! them: a digest's bytes may be deleted only while no manifest row roots it,
//! and a writer may record a root only while no delete is armed. The two
//! tables are therefore always read and written inside one transaction, which
//! is why they are one family.
//!
//! The third, [`blob`], is the bytes those rules govern when the backend is
//! the SQLite session catalog itself. It exists on SQLite only; PostgreSQL
//! takes an external attachment backend.
//!
//! # The GC predicates, and why some of them come in pairs
//!
//! Four of the family's operations reach outside it: the deleted-session root
//! reclaim, the per-session forget, the live-root probe and the aged-intent
//! forget read `deleted_sessions`, `graph_nodes`, `runtime_turn_commits` and
//! the process registry. The first two fork on a boolean literal and are
//! dialect-only; the last two are shared, and each exists **twice**.
//!
//! The pair is the deployment, not a parameter. A deployment with a process
//! registry bound can prove a process owner dead — the owning incarnation is
//! gone from `processes` — and one without it cannot, so it conservatively
//! retains process-owned rows. Those are two production shapes, so they are
//! two statements ([`manifest::ManifestStatements::select_live_root`] and
//! [`manifest::ManifestProcessOwnerStatements::select_live_root_proving_process_death`]):
//! collapsing them into `?N IS NULL OR …` would cost the index on every call
//! for a decision made once at startup.
//!
//! On SQLite the registry is a *different database*, reached through an
//! `ATTACH`ed name while the manifest stays in the session catalog, so the
//! owner-death statements are rendered for a layout that places
//! `attachment_manifest` in `main` and `processes` in `process_registry`
//! (FIG-3406). A store with no registry has no such layout, so those
//! statements cannot be rendered for it at all — the shape it must not issue
//! is unavailable rather than merely unused.

pub mod blob;
pub mod condemnation;
pub mod manifest;
