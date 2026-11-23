//! `brumm-cli`: see the library docs. Parses arguments, sets up logging and the runtime.

use std::process::ExitCode;

use clap::Parser;

/// Network work is light; two workers keep idle overhead small (as in the desktop app).
const WORKER_THREADS: usize = 2;
/// Log level without RUST_LOG: only our own warnings for commands (unreachable relays and
/// network teardown are normal for a short run), more for the daemon's log.
const COMMAND_LOG: &str = "off,brumm_core=warn,brumm_host=warn,brumm_cli=warn";
const DAEMON_LOG: &str = "warn,brumm_core=info,brumm_host=info,brumm_cli=info";

fn main() -> ExitCode {
    let cli = brumm_cli::cli::Cli::parse();
    let default_log = match cli.command {
        brumm_cli::cli::Cmd::Daemon(_) => DAEMON_LOG,
        _ => COMMAND_LOG,
    };
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(default_log)),
        )
        .with_writer(std::io::stderr)
        .init();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(WORKER_THREADS)
        .enable_all()
        .build()
        .expect("tokio runtime");
    runtime.block_on(brumm_cli::run(cli))
}
