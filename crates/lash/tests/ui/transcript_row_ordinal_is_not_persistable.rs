use lash::transcript::TranscriptRow;
fn persist(row: &TranscriptRow) {
    let _ = serde_json::to_string(&row.ordinal());
    let _: u32 = row.ordinal().into();
    let _ = serde_json::to_string(row);
}
fn main() {}
