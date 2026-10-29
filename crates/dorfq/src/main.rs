//! The `dorfq` command line.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use dorfq::config::Config;
use dorfq::driver::{Driver, DriverError};
use dorfq::providers::{self, Outcome};
use dorfq_core::{ForgeError, Observation};

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

/// Waits for the forge, then ticks, until a fatal error or, with
/// `exit_when_idle`, an idle tick. A tick is printed when it is the first,
/// plans something, or observes something other than the last one printed
/// did, the time aside.
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
    let mut printed: Option<Observation> = None;
    let mut outcome = Outcome::Done;
    loop {
        (built.wait)(outcome);
        outcome = Outcome::Done;
        match driver.tick() {
            Ok(report) => {
                if !printed.as_ref().is_some_and(|last| report.repeats(last)) {
                    println!("{report}");
                    printed = Some(report.observation.clone());
                }
                if exit_when_idle && report.idle && !(built.has_scheduled_events)() {
                    return ExitCode::SUCCESS;
                }
            }
            Err(DriverError::Forge(error @ ForgeError::Transient(_))) => {
                eprintln!("dorfq: {error}; retrying");
                outcome = Outcome::Transient;
            }
            Err(
                error @ (DriverError::Forge(ForgeError::Permanent(_)) | DriverError::Scheduler(_)),
            ) => {
                eprintln!("dorfq: {error}");
                return ExitCode::FAILURE;
            }
        }
    }
}
