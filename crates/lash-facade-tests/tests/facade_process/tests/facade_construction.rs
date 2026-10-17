#[test]
fn process_tool_definition_selects_each_capability_contract() {
    use lash::process_controls::{ProcessControlTool as Tool, process_tool_definition};
    for (tool, name) in [
        (Tool::Start, "start_process"),
        (Tool::List, "list_process_handles"),
        (Tool::Await, "await_process"),
        (Tool::Get, "get_process_definition"),
        (Tool::Cancel, "cancel_process"),
    ] {
        assert_eq!(process_tool_definition(tool).name(), name);
    }
}
