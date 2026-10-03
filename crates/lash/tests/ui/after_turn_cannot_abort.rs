use lash::plugins::AfterTurnContributions;

fn main() {
    let _ = AfterTurnContributions {
        abort: Some("after-turn observers cannot abort"),
        ..AfterTurnContributions::default()
    };
}
