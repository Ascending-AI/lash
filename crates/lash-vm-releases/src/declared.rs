//! Which helper releases the build holds (FIG-5799, FIG-5828).
//!
//! Each declared release other than the one the tree builds has shipped, and
//! is kept as it was sealed (`sealed.rs`). The release the tree builds is the
//! build's own until the cut that ships it seals it (FIG-5839): the build
//! defines it from the current helper sources, and nothing of it is checked
//! in.
//!
//! The build script includes this file too.

/// The name and ordinal of each helper release the standard embedding
/// retains, oldest first: the last is the release the tree builds.
/// Declaring the next release after a sealed one is how a build changes a
/// helper that has shipped.
pub const RETAINED_HELPER_RELEASES: &[(&str, u32)] = &[("1.0", 1)];

/// Sealed releases removed from the runnable union, kept for the startup
/// retirement survey. Moving a release's declaration here from
/// [`RETAINED_HELPER_RELEASES`] declares its retirement; it never expires by
/// time.
pub const RETIRING_HELPER_RELEASES: &[(&str, u32)] = &[];
