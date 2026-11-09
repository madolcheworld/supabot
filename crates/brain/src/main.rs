//! The binary: parse the command line, build the brain, serve, exit.
//!
//! Everything interesting lives in the library, so this file is deliberately
//! three statements long — the CLI embeds the same [`swarmbot_brain::run`] path,
//! and a second copy of the boot sequence here would be a second thing to keep
//! honest.

use std::process::ExitCode;
use std::time::Duration;

/// How long a stop is given to finish before the process leaves anyway.
///
/// [`swarmbot_core::Swarm::shutdown`] joins the runners, which finish the turn
/// they are in; a backend that is hung on a socket should not be able to keep a
/// stopped daemon alive. The budget turns "stop" into "stop, then leave".
const SHUTDOWN_BUDGET: Duration = Duration::from_secs(10);

fn main() -> ExitCode {
    let cli = swarmbot_brain::cli();

    // Built by hand rather than with `#[tokio::main]`: a daemon has to decide its
    // own exit code, and the shutdown budget below has to run *after* the future
    // completes, which the attribute macro gives no place to put it.
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_name("swarmbot-brain")
        .build()
    {
        Ok(runtime) => runtime,
        Err(err) => {
            eprintln!("swarmbot-brain: cannot start the async runtime: {err}");
            return ExitCode::FAILURE;
        }
    };

    let outcome = runtime.block_on(swarmbot_brain::run(cli));
    runtime.shutdown_timeout(SHUTDOWN_BUDGET);

    match outcome {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            // `{:#}` prints the whole anyhow chain: "cannot bind 127.0.0.1:8787:
            // address already in use" is one line an operator can act on.
            eprintln!("swarmbot-brain: {err:#}");
            ExitCode::FAILURE
        }
    }
}
