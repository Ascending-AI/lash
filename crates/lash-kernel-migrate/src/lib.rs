//! The lash kernel's version migrations (`K-VER-003` to `K-VER-005`).
//!
//! A kernel version may break a pinned rule. The version that does ships a
//! [`Migration`] from the version before it: total functions with typed
//! refusals that redeclare a library function, rewrite a document, answering
//! which node of the old document is which node of the new one
//! (`lash-kernel-edit`'s [`Correspondence`]), and carry a parked run onto
//! the rewritten document over that correspondence.
//!
//! [`carry`] is the part every migration shares: it moves each coordinate a
//! parked run saves, every site and every function identity, to where the
//! rewrite put it, and refuses the run when a coordinate has no successor.
//! A migration that changes more than coordinates rewrites the state first
//! and carries the result.
//!
//! [`migration_from`] is the catalogue of the migrations this build ships:
//! one from each version it still interprets to the one that replaced it.
//! An embedder applies it when it takes over a run parked by the previous
//! build; nothing here stores, commits or schedules.
//!
//! A saved function ([`saved_function`]) is a document of its own, so it is
//! carried by the document rewrite alone.
//!
//! The crate depends on `lash-kernel-doc`, `lash-kernel-dialect`,
//! `lash-kernel-edit` and `lash-kernel-state`, and on no other lash crate.

mod carry;
mod migration;
mod rewrite;
mod saved;
#[cfg(feature = "synthetic-next")]
mod synthetic;

pub use carry::carry;
pub use lash_kernel_edit::{Correspondence, Survivor};
pub use migration::{
    DocumentRefusal, MigrateRegistryError, Migration, ParkedRefusal, Rewritten, migrate_registry,
    migration_from,
};
pub use rewrite::{Redeclared, redeclare, replace_functions, unchanged};
pub use saved::saved_function;
