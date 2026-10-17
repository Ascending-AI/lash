use lash::tools::AttemptContext;
use lash::tools::StartProcessIntent;
use lash::tools::ToolIntents;

// A pending call declares at most one start, and only through the sealed
// constructor: it cannot be built from a batch of intents, and it cannot hold
// a second start.
fn declare(context: &AttemptContext<'_>, first: StartProcessIntent, second: StartProcessIntent) {
    let _from_intents = context.declare_start(ToolIntents::default());
    let _two_starts = context.declare_start(first, second);
}

fn main() {}
