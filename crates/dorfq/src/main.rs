//! The `dorfq` command line.

use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use clap::{Parser, Subcommand};
use dorfq::config::Config;
use dorfq::driver::{Driver, DriverError, TickReport};
use dorfq::journal::{self, Header, status};
use dorfq::providers::{self, Outcome};
use dorfq::{telemetry, unix_now};
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
        Verb::Events { .. } => events.iter().map(journal::events::line).collect(),
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
/// every tick is journaled, a fatal one before the exit; with a status
/// issue as well, each tick that changes the status document hands the
/// forge its new body; with `[telemetry]`, each tick's events and gauges are
/// handed to the exporter, which pushes them on its interval and once more
/// at exit.
fn run(config: &Path, exit_when_idle: bool) -> ExitCode {
    let config = match Config::load(config) {
        Ok(config) => config,
        Err(error) => {
            eprintln!("dorfq: {error}");
            return ExitCode::FAILURE;
        }
    };
    let started = unix_now();
    let exporter = match &config.telemetry {
        None => None,
        Some(settings) => match telemetry::http_sink(settings) {
            Ok(sink) => Some(telemetry::Exporter::start(
                Duration::from_secs(settings.interval_secs),
                started,
                sink,
            )),
            Err(error) => {
                eprintln!("dorfq: {error}");
                return ExitCode::FAILURE;
            }
        },
    };
    let status_issue = config
        .forge
        .github_settings()
        .and_then(|settings| settings.status_issue);
    // The status structure exists before the forge, so the journal's seed
    // can feed it; the status issue and the telemetry read it, and the
    // configuration refuses either without `journal`.
    let mut live = (status_issue.is_some() || exporter.is_some()).then(status::Live::new);
    let mut journal = match &config.journal {
        None => None,
        Some(path) => {
            let header = Header::start(config.forge.kind(), &config.scheduler, started);
            let seed: journal::Sink<'_> = match &mut live {
                Some(live) => &mut |observation, events| live.seed(observation, events),
                None => &mut |_, _| {},
            };
            match journal::Writer::open(
                path,
                header,
                config.events.as_deref(),
                config.capture.as_deref(),
                seed,
            ) {
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
    let mut publish = status_issue.map(|_| {
        built
            .status
            .take()
            .expect("a GitHub forge with status_issue hands over a status handle")
    });
    let mut forge_gauges = built.gauges.take();
    // TODO(journal-recovery): replay the current segment's observations
    // through this scheduler before the first live tick.
    let mut scheduler = providers::scheduler(&config.scheduler);
    let mut driver = Driver::new(built.forge.as_mut(), scheduler.as_mut());
    let mut printed: Option<Observation> = None;
    let mut outcome = Outcome::Done;
    loop {
        // TODO(correctness-signal-stop-skips-final-export-events-never-delivered):
        // return on SIGTERM or SIGINT, so the exporter's drop delivers what
        // is pending; a signal now ends the process without it.
        (built.wait)(outcome);
        outcome = Outcome::Done;
        let ticked = driver.tick();
        // The outcome is printed before it is journaled, so a journal that
        // cannot be written loses nothing from stdout and stderr.
        let report = ticked.as_ref().map_or_else(DriverError::report, Some);
        if let Some(report) = report {
            print_report(report, &mut printed);
        }
        if let Err(error) = &ticked {
            if error.is_fatal() {
                eprintln!("dorfq: {error}");
            } else {
                eprintln!("dorfq: {error}; retrying");
            }
        }
        let exiting = matches!(&ticked, Ok(report)
            if exit_when_idle && report.idle && !(built.has_scheduled_events)());
        if let Some(writer) = &mut journal {
            let at = unix_now();
            let events = match writer.record(driver.ticks() - 1, at, &ticked) {
                Ok(events) => events,
                Err(error) => {
                    eprintln!("dorfq: {error}");
                    return ExitCode::FAILURE;
                }
            };
            // A fatal tick exits without a status write: the body stays as
            // last written, saying when it last changed. Its events still
            // reach the exporter, with no gauges.
            let fatal = ticked.as_ref().is_err_and(DriverError::is_fatal);
            let mut snapshot = None;
            if let Some(live) = live.as_mut().filter(|_| !fatal) {
                live.tick(report.map(|r| &r.observation), &events);
                if let Some(document) = live.document() {
                    if let Some(publish) = &mut publish
                        && let Some(body) = live.changed(&document, dorfq::version(), started)
                    {
                        publish(body);
                    }
                    if exporter.is_some() {
                        let forge = forge_gauges.as_mut().map(|gauges| gauges());
                        snapshot = Some(telemetry::snapshot(&document, live.now(), forge));
                    }
                }
            }
            if let Some(exporter) = &exporter {
                exporter.tick(telemetry::Tick {
                    at,
                    events,
                    snapshot,
                    observed: report.is_some() && !fatal,
                });
            }
            // Last, so the journal line, the events, the status write and
            // the export are ahead of any GitHub round trip. Only after a
            // tick whose forge calls all succeeded: a failed one may have
            // lost the repository or been told to pause. The idle exit reads
            // every head still owed, since the segment that shows them may
            // have just closed.
            let captured = if exiting {
                writer.drain_capture(at, built.capture.as_mut())
            } else if ticked.is_ok() {
                writer.capture(at, built.capture.as_mut())
            } else {
                Ok(Vec::new())
            };
            match captured {
                Ok(lines) => {
                    for line in lines {
                        eprintln!("dorfq: {line}");
                    }
                }
                Err(error) => {
                    eprintln!("dorfq: {error}");
                    return ExitCode::FAILURE;
                }
            }
        }
        match ticked {
            Ok(_) if exiting => return ExitCode::SUCCESS,
            Ok(_) => {}
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
