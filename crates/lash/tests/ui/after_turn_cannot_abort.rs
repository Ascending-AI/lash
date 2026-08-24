use lash::plugins::{AbortTurnDirective, AfterTurnPluginDirective};

fn main() {
    let _ = AfterTurnPluginDirective::AbortTurn(AbortTurnDirective {
        code: "blocked".to_string(),
        message: "after-turn hooks cannot abort".to_string(),
    });
}
