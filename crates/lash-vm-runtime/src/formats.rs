//! The kernel's stored formats, declared for lash's fleet-format set (ADR
//! 0131). The kernel crates know nothing of that machinery: they state the
//! kernel versions a build interprets, and these constants are what a fleet
//! compares.
//!
//! Each is the newest kernel version this build interprets, and each moves
//! by migration (ADR 0106): a breaking kernel version ships the migration
//! from the version before it (`lash-kernel-migrate`), and a node of the
//! new build carries a document and its parked run forward when it takes
//! over a process the previous build parked. The `synthetic-next` build
//! interprets the successor only it knows.

use lash_kernel_doc::KernelVersion;

/// The kernel version a document is written in and a run is parked under.
/// Every stored kernel shape carries it, and a reader checks it before it
/// decodes the body.
#[cfg(not(feature = "synthetic-next"))]
/// version_guard(unshaped = "the kernel version is a number every stored kernel shape carries; the shapes it versions are guarded by the document, parked-state and saved-function surfaces")
/// format_manifest = "KernelVersion"
/// version_surface = "migrate"
/// version_unguarded = "kernel version: a kernel migration moves it when a node claims the process (lash-kernel-migrate), never a record upcaster"
pub const LASH_KERNEL_VERSION: u32 = 1;
/// The synthetic successor of kernel version 1.
#[cfg(feature = "synthetic-next")]
/// format_manifest = "KernelVersion"
/// version_surface = "migrate"
/// version_unguarded = "kernel version: a kernel migration moves it when a node claims the process (lash-kernel-migrate), never a record upcaster"
pub const LASH_KERNEL_VERSION: u32 = 2;

const _: () = assert!(LASH_KERNEL_VERSION == KernelVersion::NEWEST.number());

/// The JSON encoding of a kernel document, as a store holds it.
///
/// version_guard(
///     shapes(path = "crates/lash-kernel-doc/src/document.rs", cover(Document, Manifest)),
/// )
#[cfg(not(feature = "synthetic-next"))]
/// format_manifest = "KernelDocument"
/// version_surface = "migrate"
/// version_unguarded = "kernel document: a kernel migration rewrites it when a node claims the process that runs it (lash-kernel-migrate), never a record upcaster"
pub const KERNEL_DOCUMENT_SCHEMA_VERSION: u32 = 1;
/// The synthetic successor's document encoding.
#[cfg(feature = "synthetic-next")]
/// format_manifest = "KernelDocument"
/// version_surface = "migrate"
/// version_unguarded = "kernel document: a kernel migration rewrites it when a node claims the process that runs it (lash-kernel-migrate), never a record upcaster"
pub const KERNEL_DOCUMENT_SCHEMA_VERSION: u32 = 2;

const _: () = assert!(KERNEL_DOCUMENT_SCHEMA_VERSION == KernelVersion::NEWEST.number());

/// A parked run: the machine's state in the document's terms, sealed under
/// its owner, the kernel version and the document's identity. The broker
/// stamps the snapshot row that holds it with the kernel version.
///
/// version_guard(
///     shapes(path = "crates/lash-kernel-state/src/*.rs", cover(ParkedRun)),
///     shapes(path = "crates/lash-vm-protocol/src/state.rs", cover(OpaqueVmState)),
/// )
#[cfg(not(feature = "synthetic-next"))]
/// format_manifest = "KernelParkedState"
/// version_surface = "migrate"
pub const KERNEL_PARKED_STATE_VERSION: u32 = 1;
/// The synthetic successor's parked-run format.
#[cfg(feature = "synthetic-next")]
/// format_manifest = "KernelParkedState"
/// version_surface = "migrate"
pub const KERNEL_PARKED_STATE_VERSION: u32 = 2;

const _: () = assert!(KERNEL_PARKED_STATE_VERSION == KernelVersion::NEWEST.number());

/// A saved function: a function a session keeps between cells, as its code
/// (a kernel document that declares it) and the captures frozen with it.
/// The RLM session snapshot holds one per binding, and a session is created
/// with them as an input. Its document states the kernel version it was
/// written in, and the kernel migration rewrites each stored one, alone,
/// when its session is restored or seeded.
///
/// A saved function is declared in the document of each cell that uses it,
/// so it is held in the version this build's dialects lower a cell in. The
/// synthetic successor changes no dialect, so this surface does not move
/// under it.
///
/// version_guard(
///     shapes(path = "crates/lash-kernel-dialect/src/saved.rs", cover(SavedFunction)),
/// )
/// format_manifest = "KernelSavedFunction"
/// version_surface = "migrate"
pub const KERNEL_SAVED_FUNCTION_VERSION: u32 = 1;

const _: () = assert!(KERNEL_SAVED_FUNCTION_VERSION == lash_kernel_doc::KERNEL_VERSION);

/// The helper release this build's cells and processes are written against
/// (FIG-5799): the version of the `kernel-helpers` surface in its actors'
/// format sets. A build holds the functions of each earlier release it
/// retains beside its own, so a node of it decodes what a build of one of
/// those wrote, and a node of a build that holds only an earlier release
/// never claims what a build of this one wrote. A format set without the
/// surface is release 1's, the 1.0 baseline's.
pub const KERNEL_HELPER_RELEASE: u32 = lash_vm_library::HELPER_RELEASE;

/// The version of each kernel format the previous build wrote, when this
/// build still interprets it: what a process that build parked is stored
/// in, and what a node of this build carries forward.
pub fn previous_kernel_version() -> Option<u32> {
    KernelVersion::NEWEST.previous().map(KernelVersion::number)
}

/// The version of each kernel format the build before this one wrote, when
/// this build no longer interprets it: the window that build opened is
/// closed and its interpreter deleted, so a process or a session still
/// holding state in that version is stranded under this build (ADR 0115
/// §3.5, drain by release).
pub fn retired_kernel_version() -> Option<u32> {
    if previous_kernel_version().is_some() {
        return None;
    }
    KernelVersion::NEWEST
        .number()
        .checked_sub(1)
        .filter(|version| *version > 0)
}
