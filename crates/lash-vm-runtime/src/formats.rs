//! The kernel's stored formats, declared for lash's fleet-format set (ADR
//! 0131). The kernel crates know nothing of that machinery: they state the
//! kernel versions a build interprets, and these constants are what a fleet
//! compares.
//!
//! Kernel semantics and stored formats have separate compatibility witnesses.
//! Before 1.0, format changes advance their stamps automatically and require
//! reset, without migrations or old-body readers. Kernel semantic succession
//! still governs interpreter admission; its `synthetic-next` build interprets
//! the successor only it knows.

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

/// The stored token encoding of documents and library function definitions.
/// This witness is independent of the kernel's semantic version. Before 1.0,
/// an encoding change requires a reset, with no migration or old-body reader.
///
/// version_guard(
///     shapes(path = "crates/lash-kernel-doc/src/document.rs", cover(Document)),
///     items(path = "crates/lash-kernel-doc/src/document_bytes.rs", decode),
/// )
/// format_manifest = "KernelDocument"
/// version_surface = "migrate"
/// version_unguarded = "kernel token encoding: pre-1.0 changes require a reset, never a record upcaster"
pub const KERNEL_DOCUMENT_SCHEMA_VERSION: u32 = 3;

const _: () = assert!(KERNEL_DOCUMENT_SCHEMA_VERSION == lash_kernel_doc::DOCUMENT_GRAMMAR_VERSION);

/// A parked run: the machine's state as a checkpoint root page
/// (`paged::root::CheckpointRoot`, kind 1000) over the run's persistent heap
/// (`HeapRoot`), sealed under its owner, the kernel version and the
/// document's identity. The execution root that holds it states the kernel
/// version as its format. The record, tree and page codecs beneath the root
/// are the semantic page format's (`paged::source::PAGE_FORMAT`), which this
/// guard names, so a page-format change moves this version too.
///
/// A parked run carries through a checked correspondence. Its pages,
/// execution requirements and closure roots move in one owner transaction.
///
/// version_guard(
///     items(
///         path = "crates/lash-kernel-state/src/paged/root.rs",
///         CheckpointRoot, HeapRoot, RootStatus, EndedRecord, SavedMeters, CellTargetKind,
///     ),
///     file(path = "crates/lash-kernel-state/src/paged/codec.rs", cover("record!(CheckpointRoot", "record!(HeapRoot")),
///     items(path = "crates/lash-kernel-state/src/paged/source.rs", PAGE_FORMAT),
/// )
#[cfg(not(feature = "synthetic-next"))]
/// format_manifest = "KernelParkedState"
/// version_surface = "migrate"
/// version_unguarded = "paged kernel state: authenticated coordinate carry under the execution owner fence, never a record upcaster"
pub const KERNEL_PARKED_STATE_VERSION: u32 = 1;
/// The synthetic successor's parked-run format.
#[cfg(feature = "synthetic-next")]
/// format_manifest = "KernelParkedState"
/// version_surface = "migrate"
/// version_unguarded = "paged kernel state: authenticated coordinate carry under the execution owner fence, never a record upcaster"
pub const KERNEL_PARKED_STATE_VERSION: u32 = 2;

/// A saved function: a function a session keeps between cells, held in an
/// immutable FunctionBundle node with every saved function it uses. Its
/// code is a kernel document node of its own, its captures are frozen page
/// references by name, and it names the helper release it was written
/// against. Its document states the kernel version it was written in, and
/// the kernel migration rewrites each stored one, alone, when its session
/// is restored or seeded.
///
/// A saved function is declared in the document of each cell that uses it.
/// Its format stamp tracks stored bytes independently of the semantic kernel
/// version. The synthetic successor changes no saved-function encoding, so
/// this stamp does not move under it.
///
/// version_guard(
///     items(path = "crates/lash-kernel-doc/src/snapshot/collect.rs", CaptureRefusal, VALUE_KINDS),
///     items(path = "crates/lash-kernel-dialect/src/saved.rs", SavedFunction, Token, HeldAt, NotSaved),
///     items(
///         path = "crates/lash-kernel-dialect/src/saved/graph.rs",
///         DOCUMENT_NODE_KIND, FUNCTION_BUNDLE_NODE_KIND, BundleBody, MemberBody, encode,
///         decode,
///     ),
/// )
/// format_manifest = "KernelSavedFunction"
/// version_unguarded = "saved-function bundles are unversioned graph nodes; their document encoding and kernel admission govern compatibility, not record upcasters"
/// version_surface = "migrate"
pub const KERNEL_SAVED_FUNCTION_VERSION: u32 = 1;

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
