use lash::tools::{PendingCompletion, ToolAttemptOutcome, ToolIntents};

fn main() {
    let _invalid = ToolAttemptOutcome::Pending {
        pending: PendingCompletion::new(),
        intents: ToolIntents::default(),
    };
}
