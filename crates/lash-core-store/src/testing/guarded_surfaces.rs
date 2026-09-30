//! The laws every guarded surface's owner runs over its own decoders
//! (FIG-3802): each surface reads its whole supported range and lifts it to
//! the newest shape, refuses a version outside it typed without touching the
//! stored bytes, and reads immutable history as stored, forever.
//!
//! An owner hands one [`SurfaceProbe`] per [`GUARDED_SURFACES`] row it owns:
//! the surface's production writer and production decoder. The laws refuse an
//! owner that leaves a row without a probe, so a new guarded surface cannot
//! skip them. Run in N's build a surface reads its one version; run in the
//! synthetic N+1's, it reads N's through the registered lift as well.

use crate::store::{
    BlobRef, FLEET_WRITABLE_RANGE, FleetFormat, GUARDED_SURFACES, SurfaceFormat, SurfaceReads,
    WriterPin,
};

/// One guarded surface, as its owner's laws drive it.
pub struct SurfaceProbe {
    /// The surface's registered constant name.
    pub constant: &'static str,
    /// The newest version this build knows: the constant's own value.
    pub newest: u32,
    /// The stored bytes of one fixed record, written by the surface's own
    /// writer under `fleet`: a writer that consults `F` stamps the version
    /// the fleet pins it to.
    pub write: fn(FleetFormat) -> Vec<u8>,
    /// The surface's production decoder under `fleet`: the decoded record,
    /// rendered so two decodes of the same fact compare equal, or the
    /// refusal.
    pub read: fn(&[u8], FleetFormat) -> Result<String, String>,
    /// The same stored bytes stamped `version`: a version no writer of this
    /// build emits, which a law needs to show refused.
    pub restamp: fn(&[u8], u32) -> Vec<u8>,
}

impl SurfaceProbe {
    fn surface(&self) -> SurfaceFormat {
        SurfaceFormat::of(self.constant, self.newest)
    }

    /// The record written by a writer the fleet pins to `version`.
    fn written_at(&self, version: u32) -> Vec<u8> {
        (self.write)(pinned(self.constant, version))
    }
}

/// A fleet at N's epoch whose only pin holds `constant`'s writers at
/// `version`.
pub fn pinned(constant: &'static str, version: u32) -> FleetFormat {
    let pins: &'static [WriterPin] = Box::leak(Box::new([WriterPin {
        constant,
        generation: FLEET_WRITABLE_RANGE.min(),
        version,
    }]));
    FleetFormat::from_version(FLEET_WRITABLE_RANGE.min()).with_writer_pins(pins)
}

/// Every epoch this build writes under, as the fleet `F` recording it.
fn epochs() -> impl Iterator<Item = FleetFormat> {
    (FLEET_WRITABLE_RANGE.min()..=FLEET_WRITABLE_RANGE.max()).map(FleetFormat::from_version)
}

/// The probes of `owner`, one per guarded row it owns, and no other.
fn owned<'a>(owner: &str, probes: &'a [SurfaceProbe]) -> Vec<&'a SurfaceProbe> {
    let rows = GUARDED_SURFACES
        .iter()
        .filter(|row| row.owner == owner)
        .map(|row| row.constant)
        .collect::<Vec<_>>();
    let mut probed = probes
        .iter()
        .map(|probe| probe.constant)
        .collect::<Vec<_>>();
    probed.sort_unstable();
    let mut expected = rows.clone();
    expected.sort_unstable();
    assert_eq!(
        probed, expected,
        "{owner} must probe exactly the guarded surfaces it owns"
    );
    probes.iter().collect()
}

/// Law: `every_guarded_surface_decodes_its_supported_range`. Under every
/// epoch this build writes, a record written at each version the surface's
/// read window admits decodes to the record written at the newest.
pub fn every_guarded_surface_decodes_its_supported_range(owner: &str, probes: &[SurfaceProbe]) {
    for probe in owned(owner, probes) {
        for fleet in epochs() {
            let window = fleet.read_window(probe.surface());
            assert_eq!(window.newest(), probe.newest, "{}", probe.constant);
            let current =
                (probe.read)(&probe.written_at(probe.newest), fleet).unwrap_or_else(|error| {
                    panic!(
                        "{} reads its newest under F={fleet}: {error}",
                        probe.constant
                    )
                });
            let supported = window.supported();
            let mut versions = (supported.min()..=supported.max()).collect::<Vec<_>>();
            if !versions.contains(&window.recorded()) {
                versions.push(window.recorded());
            }
            for version in versions {
                let bytes = probe.written_at(version);
                let read = (probe.read)(&bytes, fleet).unwrap_or_else(|error| {
                    panic!(
                        "{} reads version {version} of its window {supported} under F={fleet}: \
                         {error}",
                        probe.constant
                    )
                });
                assert_eq!(
                    read, current,
                    "{} version {version} lifts to its newest record",
                    probe.constant
                );
            }
        }
    }
}

/// Law: `unknown_version_is_refused_with_zero_mutation`. A record stamped
/// above the newest, or below the oldest version the window admits, is
/// refused typed under every epoch, and the stored bytes are left exactly
/// as they were.
pub fn unknown_version_is_refused_with_zero_mutation(owner: &str, probes: &[SurfaceProbe]) {
    for probe in owned(owner, probes) {
        let newest = probe.written_at(probe.newest);
        for fleet in epochs() {
            let window = fleet.read_window(probe.surface());
            let mut unknown = vec![probe.newest + 1];
            if window.oldest() > 1 {
                unknown.push(window.oldest() - 1);
            }
            for version in unknown {
                let stored = (probe.restamp)(&newest, version);
                let before = stored.clone();
                let refused = (probe.read)(&stored, fleet);
                assert!(
                    refused.is_err(),
                    "{} must refuse version {version} under F={fleet}: {refused:?}",
                    probe.constant
                );
                assert_eq!(
                    stored, before,
                    "{} refused version {version} without touching the bytes",
                    probe.constant
                );
            }
        }
    }
}

/// Law: `upcast_preserves_immutable_bytes_and_hashes`. A history record
/// written at any version of its window is read through its lift as stored:
/// the bytes and their content hash are the ones written, the build's writer
/// asked for that version reproduces them byte for byte, and the read does
/// not depend on `F`, so a finalize never makes history unreadable.
pub fn upcast_preserves_immutable_bytes_and_hashes(owner: &str, probes: &[SurfaceProbe]) {
    for probe in owned(owner, probes) {
        let reads = GUARDED_SURFACES
            .iter()
            .find(|row| row.constant == probe.constant)
            .map(|row| row.reads);
        if !matches!(reads, Some(SurfaceReads::History { .. })) {
            continue;
        }
        let history = epochs()
            .map(|fleet| fleet.read_window(probe.surface()).supported())
            .collect::<Vec<_>>();
        assert!(
            history.windows(2).all(|pair| pair[0] == pair[1]),
            "{} history is read through a floor F does not move: {history:?}",
            probe.constant
        );
        let supported = history[0];
        for version in supported.min()..=supported.max() {
            let stored = probe.written_at(version);
            let hash = BlobRef::for_content(&stored);
            let reads = epochs()
                .map(|fleet| (probe.read)(&stored, fleet))
                .collect::<Vec<_>>();
            assert!(
                reads.iter().all(Result::is_ok) && reads.windows(2).all(|pair| pair[0] == pair[1]),
                "{} version {version} reads alike under every epoch: {reads:?}",
                probe.constant
            );
            assert_eq!(
                BlobRef::for_content(&stored),
                hash,
                "{} version {version} keeps its hash",
                probe.constant
            );
            assert_eq!(
                probe.written_at(version),
                stored,
                "{} version {version} is rewritten by no one: its writer reproduces it",
                probe.constant
            );
        }
    }
}
