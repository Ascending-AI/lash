// A tool call's dispatch state is the runtime's own: a body holds only the
// sealed `AttemptContext`, and the facade exports no other context to name.
fn main() {
    let _: Option<lash::tools::ToolContext<'static>> = None;
}
