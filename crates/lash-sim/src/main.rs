mod cli;
mod runners;

#[tokio::main(flavor = "current_thread")]
async fn main() {
    lash_core::panic_containment::set_loud(true);
    if let Err(err) = runners::run(cli::SimCli::parse(std::env::args().skip(1))).await {
        eprintln!("{err}");
        std::process::exit(1);
    }
}
