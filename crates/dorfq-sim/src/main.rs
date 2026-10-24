//! The `dorfq-sim` command line.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use dorfq_sim::{SimConfig, simulate};

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
}

fn main() -> ExitCode {
    match Cli::parse().command {
        Command::Run {
            config,
            trace,
            json,
        } => run(&config, trace, json.as_deref()),
    }
}

fn run(path: &Path, trace: bool, json: Option<&Path>) -> ExitCode {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) => {
            eprintln!("dorfq-sim: {}: {error}", path.display());
            return ExitCode::FAILURE;
        }
    };
    let config = match SimConfig::parse(&text) {
        Ok(config) => config,
        Err(error) => {
            eprintln!("dorfq-sim: {}: {error}", path.display());
            return ExitCode::FAILURE;
        }
    };
    let report = simulate(&config, trace || json.is_some());
    if let Some(out) = json {
        let Some(name) = path.file_stem().and_then(|s| s.to_str()) else {
            eprintln!("dorfq-sim: {}: no UTF-8 file stem", path.display());
            return ExitCode::FAILURE;
        };
        if let Err(error) = fs::write(out, report.to_json(name, &config, &text)) {
            eprintln!("dorfq-sim: {}: {error}", out.display());
            return ExitCode::FAILURE;
        }
    }
    if trace {
        for policy in &report.policies {
            println!("policy {}", policy.name);
            for line in &policy.trace {
                println!("{line}");
            }
        }
    }
    print!("{report}");
    if report.failed() {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}
