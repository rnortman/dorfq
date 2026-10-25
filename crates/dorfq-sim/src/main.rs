//! The `dorfq-sim` command line.

use std::fs;
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use dorfq_sim::{SimConfig, SweepConfig, simulate, sweep};

#[derive(Parser)]
#[command(name = "dorfq-sim")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run every policy at every replication and print the comparison.
    Run {
        /// The simulation file.
        #[arg(long)]
        config: PathBuf,
        /// Print the first replication of each policy step by step.
        #[arg(long)]
        trace: bool,
        /// Also write the report, its configuration and the first
        /// replication of each policy as a JSON document to this file.
        #[arg(long, value_name = "PATH")]
        json: Option<PathBuf>,
    },
    /// Run every point of a sweep's grid and print the grid and the summary
    /// against its baseline policy.
    Sweep {
        /// The sweep file.
        #[arg(long)]
        config: PathBuf,
        /// Also write every replication's metrics at every point, with the
        /// file and the grid, as a JSON document to this file.
        #[arg(long, value_name = "PATH")]
        json: Option<PathBuf>,
        /// Replications run at once; by default, the available parallelism.
        #[arg(long, value_name = "N")]
        jobs: Option<NonZeroUsize>,
    },
}

fn main() -> ExitCode {
    match Cli::parse().command {
        Command::Run {
            config,
            trace,
            json,
        } => run(&config, trace, json.as_deref()),
        Command::Sweep { config, json, jobs } => grid(&config, json.as_deref(), jobs),
    }
}

/// The file's text, or the error printed.
fn read(path: &Path) -> Option<String> {
    fs::read_to_string(path)
        .map_err(|error| eprintln!("dorfq-sim: {}: {error}", path.display()))
        .ok()
}

/// Writes `document` to `out`, named by `path`'s stem; false with the error
/// printed when either fails.
fn write_json(path: &Path, out: &Path, document: impl FnOnce(&str) -> String) -> bool {
    let Some(name) = path.file_stem().and_then(|s| s.to_str()) else {
        eprintln!("dorfq-sim: {}: no UTF-8 file stem", path.display());
        return false;
    };
    if let Err(error) = fs::write(out, document(name)) {
        eprintln!("dorfq-sim: {}: {error}", out.display());
        return false;
    }
    true
}

fn grid(path: &Path, json: Option<&Path>, jobs: Option<NonZeroUsize>) -> ExitCode {
    let Some(text) = read(path) else {
        return ExitCode::FAILURE;
    };
    let config = match SweepConfig::parse(&text) {
        Ok(config) => config,
        Err(error) => {
            eprintln!("dorfq-sim: {}: {error}", path.display());
            return ExitCode::FAILURE;
        }
    };
    let jobs = match jobs.map_or_else(std::thread::available_parallelism, Ok) {
        Ok(jobs) => jobs,
        Err(error) => {
            eprintln!("dorfq-sim: available parallelism: {error}; pass --jobs");
            return ExitCode::FAILURE;
        }
    };
    let report = sweep(&config, jobs);
    // The table prints before the document is written, so a bad `--json`
    // path does not discard the sweep's results.
    print!("{report}");
    let written =
        json.is_none_or(|out| write_json(path, out, |name| report.to_json(name, &config, &text)));
    if report.failed() || !written {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}

fn run(path: &Path, trace: bool, json: Option<&Path>) -> ExitCode {
    let Some(text) = read(path) else {
        return ExitCode::FAILURE;
    };
    let config = match SimConfig::parse(&text) {
        Ok(config) => config,
        Err(error) => {
            eprintln!("dorfq-sim: {}: {error}", path.display());
            return ExitCode::FAILURE;
        }
    };
    let report = simulate(&config, trace || json.is_some());
    if trace {
        for policy in &report.policies {
            println!("policy {}", policy.name);
            for line in &policy.trace {
                println!("{line}");
            }
        }
    }
    print!("{report}");
    let written =
        json.is_none_or(|out| write_json(path, out, |name| report.to_json(name, &config, &text)));
    if report.failed() || !written {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}
