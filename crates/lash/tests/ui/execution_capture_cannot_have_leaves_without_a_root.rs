use lash::plugins::{ExecutionStateCapture as Capture, LeafChange};
use std::collections::BTreeMap;

fn main() {
    let _ = Capture {
        root: None,
        components: BTreeMap::from([("execution_state/orphan".to_owned(), LeafChange::Unchanged)]),
    };
}
