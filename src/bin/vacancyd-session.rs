//! Windowless Windows entry point, sharing the CLI's agent implementation.
#![cfg_attr(windows, windows_subsystem = "windows")]

#[path = "../windows_runtime.rs"]
mod windows_runtime;

#[cfg(windows)]
#[path = "../agent/report.rs"]
mod report;
#[cfg(windows)]
pub use report::*;
#[cfg(windows)]
#[path = "../agent/client.rs"]
mod client;
#[cfg(windows)]
#[path = "../agent/lifecycle.rs"]
mod lifecycle;
#[cfg(windows)]
#[path = "../agent/session_lock.rs"]
mod session_lock;

#[cfg(windows)]
#[derive(clap::Parser)]
#[command(
    name = "vacancyd-session",
    version,
    about = "Windowless session reporter; use vacancyd agent for diagnostics"
)]
struct Args {
    // Restart Manager supplies this marker; no saved application state is needed.
    #[arg(long, hide = true)]
    restart_manager: bool,
}

#[cfg(windows)]
fn main() {
    use clap::Parser;
    // Parse before registering or starting anything: --help and invalid options
    // must never accidentally launch a background reporter.
    let Args { restart_manager: _ } = Args::parse();
    let result = lifecycle::register_restart().and_then(|()| {
        lifecycle::run(|shutdown| {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?
                .block_on(async {
                    tokio::select! {
                        result = client::run(std::time::Duration::from_secs(3), false) => result,
                        _ = shutdown => Ok(()),
                    }
                })
        })
    });
    if let Err(error) = result {
        // Use `vacancyd agent` for interactive diagnostics; never create a
        // surprise console here. Missing reports remain conservative.
        eprintln!("{error:#}");
        std::process::exit(1);
    }
}

#[cfg(not(windows))]
fn main() {
    eprintln!("vacancyd-session requires Windows");
    std::process::exit(1);
}

#[cfg(all(test, windows))]
mod argument_tests {
    use super::*;
    use clap::{error::ErrorKind, Parser};

    #[test]
    fn help_and_invalid_options_do_not_start_an_agent() {
        assert!(Args::try_parse_from(["vacancyd-session"]).is_ok());
        assert!(Args::try_parse_from(["vacancyd-session", "--restart-manager"]).is_ok());
        for (argument, expected) in [
            ("--help", ErrorKind::DisplayHelp),
            ("--version", ErrorKind::DisplayVersion),
            ("--install", ErrorKind::UnknownArgument),
        ] {
            assert_eq!(
                Args::try_parse_from(["vacancyd-session", argument])
                    .err()
                    .unwrap()
                    .kind(),
                expected
            );
        }
    }
}
