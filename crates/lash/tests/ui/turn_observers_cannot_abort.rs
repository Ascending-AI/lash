use lash::plugins::TurnContributions;

fn main() {
    let _ = TurnContributions {
        abort: Some("before-turn and checkpoint observers cannot abort"),
        ..TurnContributions::default()
    };
}
