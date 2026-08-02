use clap::{CommandFactory, FromArgMatches};
use std::process;

mod app;
mod aws;
mod cli;
mod dashboard;
mod db;
mod error;
mod output;
mod shell;
mod sql;
mod target;

#[cfg(test)]
#[path = "../tests/live/mod.rs"]
mod live_tests;

#[cfg(all(test, unix))]
fn pty_test_lock() -> &'static std::sync::Mutex<()> {
    static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
    LOCK.get_or_init(|| std::sync::Mutex::new(()))
}

fn main() {
    let matches = cli::Cli::command().get_matches();
    let cli = cli::Cli::from_arg_matches(&matches)
        .unwrap_or_else(|error| error.exit())
        .with_script_input_order(&matches);
    let runtime = match tokio::runtime::Runtime::new() {
        Ok(runtime) => runtime,
        Err(_) => {
            eprintln!("error: could not initialize the async runtime");
            process::exit(1);
        }
    };
    if let Err(error) = runtime.block_on(cli.run()) {
        if !error.is_quiet() {
            eprintln!("error: {error}");
        }
        process::exit(error.exit_code());
    }
}
