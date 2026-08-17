// A declared park announcement is re-declared on every redrive of the attempt,
// so the replay key that makes the append idempotent is required rather than
// optional: there is no way to construct one without it.
fn main() {
    let _ = lash::tools::PendingAnnouncement {
        event_type: String::new(),
        payload: Default::default(),
    };
}
