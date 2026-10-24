//! The `dorfq` command line.

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::thread;
use std::time::Duration;

use clap::{Parser, Subcommand};
use dorfq::config::Config;
use dorfq::driver::{Driver, DriverError};
use dorfq::providers;
use dorfq_core::ForgeError;

#[derive(Parser)]
#[command(name = "dorfq", version = dorfq::version())]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the queue: observe, decide and act, once per tick.
    Run {
        /// The configuration file.
        #[arg(long)]
        config: PathBuf,
        /// Stop once nothing is queued, nothing is in flight, nothing was
        /// planned and the forge has no scheduled events left.
        #[arg(long)]
        exit_when_idle: bool,
    },
}

fn main() -> ExitCode {
    match Cli::parse().command {
        Command::Run {
            config,
            exit_when_idle,
        } => run(&config, exit_when_idle),
    }
}

fn run(config: &Path, exit_when_idle: bool) -> ExitCode {
    let config = match Config::load(config) {
        Ok(config) => config,
        Err(error) => {
            eprintln!("dorfq: {error}");
            return ExitCode::FAILURE;
        }
    };
    let mut built = match providers::forge(&config.forge) {
        Ok(built) => built,
        Err(error) => {
            eprintln!("dorfq: {error}");
            return ExitCode::FAILURE;
        }
    };
    let mut scheduler = providers::scheduler(&config.scheduler);
    let mut driver = Driver::new(built.forge.as_mut(), scheduler.as_mut());
    let pause = Duration::from_millis(config.driver.tick_millis);
    loop {
        match driver.tick() {
            Ok(report) => {
                println!("{report}");
                if exit_when_idle && report.idle && !(built.has_scheduled_events)() {
                    return ExitCode::SUCCESS;
                }
            }
            Err(DriverError::Forge(error @ ForgeError::Transient(_))) => {
                eprintln!("dorfq: {error}; retrying")
            }
            Err(
                error @ (DriverError::Forge(ForgeError::Permanent(_)) | DriverError::Scheduler(_)),
            ) => {
                eprintln!("dorfq: {error}");
                return ExitCode::FAILURE;
            }
        }
        thread::sleep(pause);
    }
}
