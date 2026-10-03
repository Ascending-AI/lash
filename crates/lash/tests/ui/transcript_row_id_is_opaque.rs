use lash::transcript::TranscriptRow;
fn correlate(row: &TranscriptRow) {
    let _ = row.row_id().strip_prefix("host-owned:");
}
fn main() {}
