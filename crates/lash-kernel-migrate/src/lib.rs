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
//! [`adopt`] plans an explicit rewrite within one kernel version, refused
//! when it changes an operation contract. Build helpers keep exact FunctionIds;
//! ADR 0149 reserves their upgrade pairing for the permanent HelperId ledger.
//! A helper release alone never rewrites an existing publication or run.
//!
//! [`carry_run`] rewrites a parked bytecode-core run onto its successor's
//! code. The embedding validates the foreign continuation and commits its
//! pages, requirements and roots in one fenced transaction. An unmapped id
//! refuses; the original root remains readable.
//!
//! The crate depends on `lash-kernel-doc`, `lash-kernel-check` (an
//! adopted document's requirements are derived, not copied),
//! `lash-kernel-dialect` and `lash-kernel-state`, and on no other lash
//! crate.

mod adopt;
mod migration;
mod parked;
pub use parked::{CarryContext, CarryFault};
mod rewrite;
mod run;
pub use run::{Successor, carry_effect, carry_run, run_code};
mod saved;
#[cfg(feature = "synthetic-next")]
mod synthetic;

pub use adopt::{AdoptedUses, AdoptionRefusal, adopt};
pub use migration::{
    DocumentRefusal, KernelMigrationRefusal, MigrateRegistryError, Migration, ParkedRefusal,
    Rewritten, migrate_registry, migration_from,
};
pub use rewrite::{Redeclared, redeclare, replace_functions, unchanged};
pub use saved::{function_value, saved_function};

mod helper_upgrade;
pub use helper_upgrade::{
    check_run_upgrade, check_upgrade_captures, check_upgrade_data, upgrade_callable,
    upgrade_helpers,
};
