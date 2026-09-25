pub struct SimCli {
    pub command: SimCommand,
}

pub enum SimCommand {
    FixedScripts(Vec<String>),
    Run(Vec<String>),
    Replay(Vec<String>),
    BackendContention(Vec<String>),
    BackendFaults(Vec<String>),
    StackProbe(Vec<String>),
    Minimize(Vec<String>),
    Help,
    Unknown(String, Vec<String>),
}
impl SimCli {
    pub fn parse(mut args: impl Iterator<Item = String>) -> Self {
        let Some(command) = args.next() else {
            return Self {
                command: SimCommand::Help,
            };
        };
        let rest = args.collect();
        let command = match command.as_str() {
            "fixed-scripts" => SimCommand::FixedScripts(rest),
            "run" => SimCommand::Run(rest),
            "replay" => SimCommand::Replay(rest),
            "backend-contention" => SimCommand::BackendContention(rest),
            // `sqlite-faults` predates the PostgreSQL lane and stays a working
            // alias: the confidence gate and the README both invoke it by name.
            "backend-faults" | "sqlite-faults" => SimCommand::BackendFaults(rest),
            "stack-probe" => SimCommand::StackProbe(rest),
            "minimize" => SimCommand::Minimize(rest),
            "-h" | "--help" => SimCommand::Help,
            other => SimCommand::Unknown(other.to_string(), rest),
        };
        Self { command }
    }
}
