// FIG-3588: a Restate process segment reaches its effects only through the
// proof that its start marker committed. The proof has no public
// constructor, so a host cannot mint one and skip the marker: the segment's
// effect controller and its runner both require it.
use lash_core::{AdmittedScope, ProcessExecutionWriteAuthority};
use lash_restate::SegmentStarted;

fn forge(admitted: AdmittedScope, authority: ProcessExecutionWriteAuthority) -> SegmentStarted {
    SegmentStarted {
        admitted,
        segment_ordinal: 0,
        authority,
    }
}

fn main() {
    let _ = forge;
}
