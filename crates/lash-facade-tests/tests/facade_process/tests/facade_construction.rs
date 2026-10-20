#[test]
fn process_tool_definition_selects_each_capability_contract() {
    use lash::process_controls::{ProcessControlTool as Tool, process_tool_definition};
    for (tool, name) in [
        (Tool::Start, "processes.start"),
        (Tool::List, "processes.list"),
        (Tool::Await, "processes.await"),
        (Tool::Get, "processes.get"),
        (Tool::Cancel, "processes.cancel"),
    ] {
        assert_eq!(process_tool_definition(tool).manifest.tool(), name);
    }
}
