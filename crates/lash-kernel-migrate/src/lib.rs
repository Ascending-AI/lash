//! The lash kernel's version migrations (`K-VER-003`, `K-VER-004`).
//!
//! A kernel version may break a pinned rule. The version that does ships a
//! [`Migration`] from the version before it: total functions with typed
//! refusals that redeclare a library function and rewrite a document,
//! answering which node of the old document is which node of the new one
//! ([`lash_kernel_doc::Correspondence`]).
//!
//! [`migration_from`] is the catalogue of the migrations this build ships:
//! one from each version it still interprets to the one that replaced it.
//! An embedder applies it to the documents and definitions the previous
//! build wrote; nothing here stores, commits or schedules.
//!
//! A saved function ([`saved_function`]) is a document of its own, so it is
//! carried by the document rewrite alone, as a function value a host holds
//! is ([`function_value`]).
//!
//! [`adopt`] is a rewrite within one version: a document relinked by name
//! from the releases of the packages it calls to their successors
//! ([`relink`], `K-LIB-009`), refused when that would move it onto a
//! changed operation contract.
//!
//! A parked run is not carried. Its state is pages of the persistent heap
//! (ADR 0145), and this crate holds no migrator for them: an embedder
//! refuses such a run with [`ParkedRefusal::StateNotCarried`], keeps it,
//! and resumes it on a build that interprets what it pins. The paged
//! parked-run migrator is FIG-6233.
//!
//! The crate depends on `lash-kernel-doc`, `lash-kernel-check` (an
//! adopted document's requirements are derived, not copied),
//! `lash-kernel-dialect` and `lash-kernel-state`, and on no other lash
//! crate.

mod adopt;
mod migration;
mod rewrite;
mod saved;
#[cfg(feature = "synthetic-next")]
mod synthetic;

pub use adopt::{Relink, adopt, relink};
pub use migration::{
    DocumentRefusal, KernelMigrationRefusal, MigrateRegistryError, Migration, ParkedRefusal,
    Rewritten, migrate_registry, migration_from,
};
pub use rewrite::{Redeclared, redeclare, replace_functions, unchanged};
pub use saved::{function_value, saved_function};
