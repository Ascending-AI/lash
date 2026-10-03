use lash::plugins::{AfterToolDecision, CachedToolSuccess};
use lash::tools::ToolValue;

fn main() {
    let _ = AfterToolDecision::Cached(CachedToolSuccess::new(ToolValue::untrusted_json(
        serde_json::json!("a replacement result"),
    )));
}
