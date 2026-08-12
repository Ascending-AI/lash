use lash::tools::{PendingCompletion, ToolAttemptResult, ToolIntents};

fn main() {
    let _invalid = ToolAttemptResult::Pending {
        pending: PendingCompletion::new(),
        intents: ToolIntents::default(),
    };
}
