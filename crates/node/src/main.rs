//! `swarmbot-node` — the daemon entry point. Thin on purpose: everything lives
//! in the library so an integration test (or the brain's own test harness) can
//! drive a node without spawning a process.

use std::process::ExitCode;

/// Parse, log, dispatch. The exit code is the only thing `main` decides:
/// `Ok` is 0, any [`swarmbot_node::NodeError`] is 1 with the error printed to
/// stderr — including a pairing the brain refused, whose message is the brain's
/// own reason.
#[tokio::main]
async fn main() -> ExitCode {
    let cli = swarmbot_node::cli::parse();
    swarmbot_node::init_tracing(cli.verbose);
    match swarmbot_node::cli::execute(cli).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("swarmbot-node: {error}");
            ExitCode::FAILURE
        }
    }
}
