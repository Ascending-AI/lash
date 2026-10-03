use lash::messages::PartKind;
fn classify(kind: PartKind) {
    match kind {
        PartKind::Text | PartKind::Attachment | PartKind::Code | PartKind::Output
        | PartKind::Error | PartKind::Prose | PartKind::ToolCall | PartKind::ToolResult
        | PartKind::Reasoning => (),
    }
}
fn main() {}
