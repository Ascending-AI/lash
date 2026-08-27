pub struct SimCli {
    pub command: SimCommand,
}

pub enum SimCommand {
    FixedScripts(Vec<String>),
    Run(Vec<String>),
    RunPostgres(Vec<String>),
    Replay(Vec<String>),
    ReplaySqlite(Vec<String>),
    ReplayPostgres(Vec<String>),
    BackendContention(Vec<String>),
    SqliteFaults(Vec<String>),
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
            "run-postgres" => SimCommand::RunPostgres(rest),
            "replay" => SimCommand::Replay(rest),
            "replay-sqlite" => SimCommand::ReplaySqlite(rest),
            "replay-postgres" => SimCommand::ReplayPostgres(rest),
            "backend-contention" => SimCommand::BackendContention(rest),
            "sqlite-faults" => SimCommand::SqliteFaults(rest),
            "stack-probe" => SimCommand::StackProbe(rest),
            "minimize" => SimCommand::Minimize(rest),
            "-h" | "--help" => SimCommand::Help,
            other => SimCommand::Unknown(other.to_string(), rest),
        };
        Self { command }
    }
}
