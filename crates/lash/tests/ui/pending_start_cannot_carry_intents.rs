use lash::tools::{AttemptContext, DeclaredStart, StartProcessIntent, ToolIntents};

// A pending call declares at most one start, and only through the sealed
// constructor: it cannot be built from a batch of intents, and it cannot hold
// a second start.
fn declare(context: &AttemptContext<'_>, first: StartProcessIntent, second: StartProcessIntent) {
    let _from_intents = DeclaredStart::new(context, ToolIntents::default());
    let _two_starts = DeclaredStart::new(context, first, second);
}

fn main() {}
