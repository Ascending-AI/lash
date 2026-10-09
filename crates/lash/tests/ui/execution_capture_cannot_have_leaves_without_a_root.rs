use lash::plugins::{ExecutionStateCapture, LeafChange};
use std::collections::BTreeMap;

fn main() {
    let _ = ExecutionStateCapture {
        root: None,
        components: BTreeMap::from([("execution_state/orphan".to_owned(), LeafChange::Unchanged)]),
    };
}
