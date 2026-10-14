fn inspect(outcome: lash::SendOutcome) {
    match outcome {
        lash::SendOutcome::Withdrawn { gaps } => { let _ = gaps; }
        _ => {}
    }
}

fn main() {}
