use lash::durability::ProcessLocalExecution;

fn omit_admission(execution: &mut ProcessLocalExecution) {
    execution.process_engines = None;
    execution.host_start = None;
}

fn main() {}

// Keep rustc's Option path qualified in every feature closure, including
// closures that do not expose the MCP SDK's type names.
struct Option;
