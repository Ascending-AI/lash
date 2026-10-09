//! The kernel's stored formats, declared for lash's fleet-format set (ADR
//! 0131). The kernel crates know nothing of that machinery: they state one
//! kernel version, and these constants are what a fleet compares.
//!
//! Stored shapes change in place at version 1 until the 1.0 freeze ends.

/// The kernel version a document is written in and a run is parked under.
/// Every stored kernel shape carries it, and a reader checks it before it
/// decodes the body.
///
/// version_surface = "coexist"
pub const LASH_KERNEL_VERSION: u32 = 1;

const _: () = assert!(LASH_KERNEL_VERSION == lash_kernel_doc::KERNEL_VERSION);

/// The JSON encoding of a kernel document, as a store holds it.
///
/// version_guard(
///     shapes(path = "crates/lash-kernel-doc/src/document.rs", cover(Document, Manifest)),
/// )
/// version_surface = "coexist"
pub const KERNEL_DOCUMENT_SCHEMA_VERSION: u32 = 1;

const _: () = assert!(KERNEL_DOCUMENT_SCHEMA_VERSION == lash_kernel_doc::KERNEL_VERSION);

/// A parked run: the machine's state in the document's terms, sealed under
/// its owner, the kernel version and the document's identity. The broker
/// stamps the snapshot row that holds it with the kernel version.
///
/// version_guard(
///     shapes(path = "crates/lash-kernel-state/src/*.rs", cover(ParkedRun)),
///     shapes(path = "crates/lash-vm-protocol/src/state.rs", cover(OpaqueVmState)),
/// )
/// version_surface = "coexist"
pub const KERNEL_PARKED_STATE_VERSION: u32 = 1;

const _: () = assert!(KERNEL_PARKED_STATE_VERSION == lash_kernel_doc::KERNEL_VERSION);
