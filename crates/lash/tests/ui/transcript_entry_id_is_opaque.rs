use lash::transcript::TranscriptEntry;
fn correlate(entry: &TranscriptEntry) {
    let _ = entry.entry_id.strip_prefix("host-owned:");
}
fn main() {}
