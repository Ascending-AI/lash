use lash::tools::{DeclaredStart, StartProcessIntent, ToolIntentIdentity};

// Only `AttemptContext::declare_start` mints a declared start, against the attempt that
// declares it; a literal cannot forge one.
fn forge(start: StartProcessIntent, identity: ToolIntentIdentity) -> DeclaredStart {
    DeclaredStart {
        start: Box::new(start),
        identity: Box::new(identity),
    }
}

fn main() {
    let _ = forge;
}
