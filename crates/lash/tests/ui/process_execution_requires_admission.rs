use lash::durability::ProcessLocalExecution;

fn omit_admission(execution: &mut ProcessLocalExecution) {
    execution.process_engines = None;
    execution.host_start = None;
}

fn main() {}
