use lash::postgres::PostgresHost;

fn forge(host: PostgresHost) -> PostgresHost {
    PostgresHost {
        storage: host.storage,
        live_replay: host.live_replay,
        process_replay: host.process_replay,
    }
}

fn main() { let _ = forge; }
