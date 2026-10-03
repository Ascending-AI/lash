// K10: only a tool body and a before-turn, after-turn, checkpoint or
// after-tool callback return state commands, published with their recorded
// result. A decision-only hook's answer carries no commands.
use lash::plugins::{AssistantResponseTransform, StateCommands};

fn derive(response: lash::provider::LlmResponse) -> AssistantResponseTransform {
    AssistantResponseTransform {
        response,
        events: Vec::new(),
        state: StateCommands::new(),
    }
}

fn main() {}
