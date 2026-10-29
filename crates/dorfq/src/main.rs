//! The `dorfq` command line.

use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{SystemTime, UNIX_EPOCH};

use clap::{Parser, Subcommand};
use dorfq::config::Config;
use dorfq::driver::{Driver, DriverError, TickReport};
use dorfq::journal::{self, Header};
use dorfq::providers::{self, Outcome};
use dorfq_core::Observation;

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
    /// Read a journal's segments, in the order given: the closed ones
    /// (`<path>.*`) oldest first, then the current one.
    Journal {
        #[command(subcommand)]
        verb: Verb,
    },
}

#[derive(Subcommand)]
enum Verb {
    /// Print the event stream, one JSON object per line.
    Events {
        #[arg(required = true)]
        segments: Vec<PathBuf>,
    },
    /// Print the summary, as a table or one JSON object.
    Metrics {
        /// Print the summary as one JSON object.
        #[arg(long)]
        json: bool,
        #[arg(required = true)]
        segments: Vec<PathBuf>,
    },
    /// Print a report document the simulation page opens.
    Report {
        /// The name the page shows.
        #[arg(long, default_value = "journal")]
        name: String,
        #[arg(required = true)]
        segments: Vec<PathBuf>,
    },
}

fn main() -> ExitCode {
    match Cli::parse().command {
        Command::Run {
            config,
            exit_when_idle,
        } => run(&config, exit_when_idle),
        Command::Journal { verb } => read_journal(verb),
    }
}

/// Reads the segments `verb` names and prints what it asks for; a segment
/// that cannot be read exits 1 naming its path and line.
fn read_journal(verb: Verb) -> ExitCode {
    let segments = match &verb {
        Verb::Events { segments }
        | Verb::Metrics { segments, .. }
        | Verb::Report { segments, .. } => segments,
    };
    let read = match journal::read(segments) {
        Ok(read) => read,
        Err(error) => {
            eprintln!("dorfq: {error}");
            return ExitCode::FAILURE;
        }
    };
    for note in &read.notes {
        eprintln!("dorfq: {note}");
    }
    let events = journal::events::derive(&read.entries);
    let text = match &verb {
        Verb::Events { .. } => events
            .iter()
            .map(|event| serde_json::to_string(event).expect("an event serialises") + "\n")
            .collect(),
        Verb::Metrics { json, .. } => {
            let summary = journal::summary::summarise(&events);
            if *json {
                serde_json::to_string(&summary).expect("a summary serialises") + "\n"
            } else {
                summary.to_string()
            }
        }
        Verb::Report { name, .. } => {
            let Some(header) = read.entries.iter().find_map(|entry| match entry {
                journal::Entry::Header(header) => Some(header),
                journal::Entry::Tick(_) => None,
            }) else {
                eprintln!("dorfq: the journal holds no header");
                return ExitCode::FAILURE;
            };
            let summary = journal::summary::summarise(&events);
            match journal::report::document(name, segments, header, &events, &summary) {
                Ok(document) => document + "\n",
                Err(error) => {
                    eprintln!(
                        "dorfq: the journal's scheduler table does not render as TOML: {error}"
                    );
                    return ExitCode::FAILURE;
                }
            }
        }
    };
    let mut stdout = io::stdout().lock();
    match stdout
        .write_all(text.as_bytes())
        .and_then(|()| stdout.flush())
    {
        Ok(()) => ExitCode::SUCCESS,
        // A reader that stopped early, as `head` does, wanted no more.
        Err(error) if error.kind() == io::ErrorKind::BrokenPipe => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("dorfq: stdout: {error}");
            ExitCode::FAILURE
        }
    }
}

/// Waits for the forge, then ticks, until a fatal error or, with
/// `exit_when_idle`, an idle tick. A tick is printed when it is the first,
/// plans something, or observes something other than the last one printed
/// did, the time aside; a tick whose plan an effect error cut short is
/// printed by the same rule, before the error. With a journal configured,
/// every tick is journaled, a fatal one before the exit.
fn run(config: &Path, exit_when_idle: bool) -> ExitCode {
    let config = match Config::load(config) {
        Ok(config) => config,
        Err(error) => {
            eprintln!("dorfq: {error}");
            return ExitCode::FAILURE;
        }
    };
    let mut journal = match &config.journal {
        None => None,
        Some(path) => {
            let header = Header::start(config.forge.kind(), &config.scheduler, unix_now());
            match journal::Writer::open(path, header) {
                Ok((writer, notes)) => {
                    for note in notes {
                        eprintln!("dorfq: {note}");
                    }
                    Some(writer)
                }
                Err(error) => {
                    eprintln!("dorfq: {error}");
                    return ExitCode::FAILURE;
                }
            }
        }
    };
    let mut built = match providers::forge(&config.forge) {
        Ok(built) => built,
        Err(error) => {
            eprintln!("dorfq: {error}");
            return ExitCode::FAILURE;
        }
    };
    // TODO(journal-recovery): replay the current segment's observations
    // through this scheduler before the first live tick.
    let mut scheduler = providers::scheduler(&config.scheduler);
    let mut driver = Driver::new(built.forge.as_mut(), scheduler.as_mut());
    let mut printed: Option<Observation> = None;
    let mut outcome = Outcome::Done;
    loop {
        (built.wait)(outcome);
        outcome = Outcome::Done;
        let ticked = driver.tick();
        // The outcome is printed before it is journaled, so a journal that
        // cannot be written loses nothing from stdout and stderr.
        match &ticked {
            Ok(report) => print_report(report, &mut printed),
            Err(error) => {
                if let DriverError::Effect { report, .. } = error {
                    print_report(report, &mut printed);
                }
                if error.is_fatal() {
                    eprintln!("dorfq: {error}");
                } else {
                    eprintln!("dorfq: {error}; retrying");
                }
            }
        }
        if let Some(writer) = &mut journal
            && let Err(error) = writer.record(driver.ticks() - 1, unix_now(), &ticked)
        {
            eprintln!("dorfq: {error}");
            return ExitCode::FAILURE;
        }
        match ticked {
            Ok(report) => {
                if exit_when_idle && report.idle && !(built.has_scheduled_events)() {
                    return ExitCode::SUCCESS;
                }
            }
            Err(error) if error.is_fatal() => return ExitCode::FAILURE,
            Err(_) => outcome = Outcome::Transient,
        }
    }
}

/// Prints `report` unless it repeats the last one printed.
fn print_report(report: &TickReport, printed: &mut Option<Observation>) {
    if !printed.as_ref().is_some_and(|last| report.repeats(last)) {
        println!("{report}");
        *printed = Some(report.observation.clone());
    }
}

/// The host clock, Unix seconds.
fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("the host clock is after 1970")
        .as_secs()
}
