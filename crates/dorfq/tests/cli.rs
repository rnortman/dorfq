//! Runs the built `dorfq` binary. Bazel supplies its path through
//! `DORFQ_BIN` and the shipped example configuration's through
//! `DORFQ_EXAMPLE_CONFIG`; this test does not build under `cargo test`.

use std::fs;
use std::io::{ErrorKind, Read as _, Write as _};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use serde_json::Value;

#[test]
fn version_flag_prints_name_and_version() {
    let output = Command::new(env!("DORFQ_BIN"))
        .arg("--version")
        .output()
        .expect("spawn dorfq");
    assert!(output.status.success(), "dorfq --version: {output:?}");
    let stdout = String::from_utf8(output.stdout).expect("stdout is UTF-8");
    let expected = format!("dorfq {}", env!("CARGO_PKG_VERSION"));
    assert!(
        stdout.starts_with(&expected),
        "expected {expected:?}, got {stdout:?}"
    );
}

#[test]
fn no_arguments_is_a_usage_error() {
    let output = Command::new(env!("DORFQ_BIN"))
        .output()
        .expect("spawn dorfq");
    // clap exits 2 on a usage error.
    assert_eq!(output.status.code(), Some(2), "dorfq: {output:?}");
    assert!(output.stdout.is_empty(), "dorfq: {output:?}");
}

#[test]
fn unknown_argument_is_rejected() {
    let output = Command::new(env!("DORFQ_BIN"))
        .arg("--nope")
        .output()
        .expect("spawn dorfq");
    // clap exits 2 on a usage error.
    assert_eq!(output.status.code(), Some(2), "dorfq --nope: {output:?}");
    assert!(output.stdout.is_empty(), "dorfq --nope: {output:?}");
    let stderr = String::from_utf8(output.stderr).expect("stderr is UTF-8");
    assert!(stderr.contains("--nope"), "dorfq --nope stderr: {stderr:?}");
}

#[test]
fn shipped_example_runs_to_idle() {
    let output = Command::new(env!("DORFQ_BIN"))
        .args([
            "run",
            "--config",
            env!("DORFQ_EXAMPLE_CONFIG"),
            "--exit-when-idle",
        ])
        .output()
        .expect("spawn dorfq");
    assert!(output.status.success(), "dorfq run: {output:?}");
    let stdout = String::from_utf8(output.stdout).expect("stdout is UTF-8");
    // The mock's commit 6 is batch 1's tip, which lands.
    let tip = format!("{:040x}", 6);
    for pr in 1..=3 {
        let line = format!("message to pr {pr}: landed as {tip}");
        assert!(stdout.contains(&line), "missing {line:?} in {stdout}");
    }
    assert!(
        stdout.contains(&format!("trunk {tip}")),
        "trunk never reached {tip}: {stdout}"
    );
    assert!(
        stdout.contains("message to pr 5: ejected: batch 2 failed"),
        "{stdout}"
    );
    let last = stdout.lines().last().expect("a trace");
    assert!(last.ends_with("| idle"), "last line {last:?}");
}

#[test]
fn unknown_kind_exits_one_naming_it() {
    let dir = PathBuf::from(std::env::var("TEST_TMPDIR").expect("TEST_TMPDIR under bazel test"));
    let config = dir.join("unknown-kind.toml");
    fs::write(
        &config,
        "[forge]\nkind = \"gitlab\"\n\n[scheduler]\nkind = \"all-at-once\"\n",
    )
    .expect("write config");
    let output = Command::new(env!("DORFQ_BIN"))
        .args(["run", "--config"])
        .arg(&config)
        .output()
        .expect("spawn dorfq");
    assert_eq!(output.status.code(), Some(1), "dorfq run: {output:?}");
    let stderr = String::from_utf8(output.stderr).expect("stderr is UTF-8");
    assert!(stderr.contains("gitlab"), "stderr: {stderr:?}");
}

#[test]
fn permanent_forge_error_exits_one() {
    // The mock writes its messages to stdout; with the reading end closed the
    // first message fails, which the mock reports as a permanent error.
    let (reader, writer) = std::io::pipe().expect("pipe");
    drop(reader);
    let output = Command::new(env!("DORFQ_BIN"))
        .args([
            "run",
            "--config",
            env!("DORFQ_EXAMPLE_CONFIG"),
            "--exit-when-idle",
        ])
        .stdout(writer)
        .output()
        .expect("spawn dorfq");
    assert_eq!(output.status.code(), Some(1), "dorfq run: {output:?}");
    let stderr = String::from_utf8(output.stderr).expect("stderr is UTF-8");
    assert!(stderr.starts_with("dorfq: "), "stderr: {stderr:?}");
    assert!(!stderr.contains("retrying"), "stderr: {stderr:?}");
}

#[test]
fn exit_when_idle_waits_for_a_later_arrival() {
    let dir = PathBuf::from(std::env::var("TEST_TMPDIR").expect("TEST_TMPDIR under bazel test"));
    let scenario = dir.join("late-arrival.toml");
    fs::write(
        &scenario,
        "ci_ticks = 1\n\n[[pr]]\nnumber = 1\n\n[[pr]]\nnumber = 2\narrives_at = 6\n",
    )
    .expect("write scenario");
    let config = dir.join("late-arrival-config.toml");
    fs::write(
        &config,
        format!(
            "[forge]\nkind = \"mock\"\nscenario = {:?}\n\n[scheduler]\nkind = \"all-at-once\"\n",
            scenario.display().to_string()
        ),
    )
    .expect("write config");
    let output = Command::new(env!("DORFQ_BIN"))
        .args(["run", "--config"])
        .arg(&config)
        .arg("--exit-when-idle")
        .output()
        .expect("spawn dorfq");
    assert!(output.status.success(), "dorfq run: {output:?}");
    let stdout = String::from_utf8(output.stdout).expect("stdout is UTF-8");
    assert!(stdout.contains("message to pr 2: landed as"), "{stdout}");
    let last = stdout.lines().last().expect("a trace");
    assert!(last.ends_with("| idle"), "last line {last:?}");
    // Each tick line, by its number and what it shows past the time, and
    // whether a plan follows it.
    let lines: Vec<&str> = stdout.lines().collect();
    let ticks: Vec<(u64, &str, bool)> = lines
        .iter()
        .enumerate()
        .filter_map(|(i, line)| {
            let rest = line.strip_prefix("tick ")?;
            let (number, rest) = rest.split_once(" now ")?;
            let (_, shown) = rest.split_once(" trunk ")?;
            let planned = lines.get(i + 1).is_some_and(|next| next.starts_with("  "));
            Some((number.parse().expect("a tick number"), shown, planned))
        })
        .collect();
    for pair in ticks.windows(2) {
        let ((_, earlier, planned), (_, later, _)) = (pair[0], pair[1]);
        assert!(
            planned || earlier != later,
            "a repeated observation was printed: {later:?} in {stdout}"
        );
    }
    // The idle stretch before the arrival repeats; its ticks ran unprinted.
    let (final_tick, _, _) = *ticks.last().expect("a tick line");
    assert!(
        final_tick + 1 > ticks.len() as u64,
        "every tick was printed: {stdout}"
    );
}

#[test]
fn github_with_unset_token_variable_exits_one_naming_it() {
    let var = "DORFQ_CLI_TEST_TOKEN_THAT_IS_NEVER_SET";
    exits_one_naming_token_variable(var, "github-unset-token.toml", |command| {
        command.env_remove(var);
    });
}

#[test]
fn github_with_empty_token_variable_exits_one_naming_it() {
    let var = "DORFQ_CLI_TEST_TOKEN_THAT_IS_EMPTY";
    exits_one_naming_token_variable(var, "github-empty-token.toml", |command| {
        command.env(var, "");
    });
}

/// Writes a `github` configuration with `api_base` nothing listens on and
/// the given `[forge.auth]` body to `file` under the test's temporary
/// directory, and returns its path.
fn github_config(file: &str, auth: &str) -> PathBuf {
    let dir = PathBuf::from(std::env::var("TEST_TMPDIR").expect("TEST_TMPDIR under bazel test"));
    let config = dir.join(file);
    fs::write(
        &config,
        format!(
            "[forge]\nkind = \"github\"\napi_base = \"https://127.0.0.1:1\"\n\
             owner = \"alice\"\nrepo = \"widgets\"\ntrunk = \"main\"\nlabel = \"queue\"\n\
             checks = [\"check\"]\n\n\
             [forge.refresh]\nlisting_secs = 60\npr_secs = 120\nbatch_secs = 30\n\n\
             [forge.auth]\n{auth}\n\
             [scheduler]\nkind = \"all-at-once\"\n"
        ),
    )
    .expect("write config");
    config
}

/// Runs `dorfq run` on `config` and expects exit 1, no stdout, and `named`
/// on stderr.
fn run_exits_one_naming(config: &Path, named: &str, prepare: impl FnOnce(&mut Command)) {
    let mut command = Command::new(env!("DORFQ_BIN"));
    command.args(["run", "--config"]).arg(config);
    prepare(&mut command);
    let output = command.output().expect("spawn dorfq");
    assert_eq!(output.status.code(), Some(1), "dorfq run: {output:?}");
    assert!(output.stdout.is_empty(), "dorfq run: {output:?}");
    let stderr = String::from_utf8(output.stderr).expect("stderr is UTF-8");
    assert!(stderr.contains(named), "stderr: {stderr:?}");
}

/// Runs `dorfq run` on a `github` configuration whose token is read from
/// `var`, and expects exit 1, no stdout, and `var` named on stderr.
fn exits_one_naming_token_variable(var: &str, file: &str, set_token: impl FnOnce(&mut Command)) {
    let config = github_config(file, &format!("kind = \"token\"\nenv = \"{var}\"\n"));
    run_exits_one_naming(&config, var, set_token);
}

#[test]
fn github_app_with_missing_key_file_exits_one_naming_it() {
    let config = github_config(
        "github-app-missing-key.toml",
        "kind = \"app\"\nclient_id = \"Iv23liTEST\"\n\
         private_key_file = \"no-such-app-key.pem\"\n",
    );
    // The relative path resolves against the configuration's directory.
    let key = config.with_file_name("no-such-app-key.pem");
    run_exits_one_naming(&config, &key.display().to_string(), |_| {});
}

/// A configuration in its own directory under `TEST_TMPDIR`, playing the
/// shipped scenario with `journal = "q.journal"` and `events = "q.events"`;
/// returns the configuration and the directory.
fn journaled_config(name: &str) -> (PathBuf, PathBuf) {
    let dir = PathBuf::from(std::env::var("TEST_TMPDIR").expect("TEST_TMPDIR under bazel test"))
        .join(name);
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("create dir");
    let example = std::env::current_dir()
        .expect("cwd")
        .join(env!("DORFQ_EXAMPLE_CONFIG"));
    let scenario = example.with_file_name("scenario.toml");
    let config = dir.join("dorfq.toml");
    fs::write(
        &config,
        format!(
            "journal = \"q.journal\"\nevents = \"q.events\"\n\n[forge]\nkind = \"mock\"\nscenario = {:?}\n\n[scheduler]\nkind = \"all-at-once\"\n",
            scenario.display().to_string()
        ),
    )
    .expect("write config");
    (config, dir)
}

#[test]
fn a_journaled_run_leaves_a_closed_segment_and_a_current_one() {
    let (config, dir) = journaled_config("journaled-run");
    let output = Command::new(env!("DORFQ_BIN"))
        .args(["run", "--config"])
        .arg(&config)
        .arg("--exit-when-idle")
        .output()
        .expect("spawn dorfq");
    assert!(output.status.success(), "dorfq run: {output:?}");
    let mut names: Vec<String> = fs::read_dir(&dir)
        .expect("read dir")
        .map(|e| e.expect("entry").file_name().into_string().expect("UTF-8"))
        .filter(|name| name.starts_with("q.journal"))
        .collect();
    names.sort();
    assert_eq!(names.len(), 2, "{names:?}");
    assert_eq!(names[0], "q.journal");
    assert!(names[1].starts_with("q.journal."), "{names:?}");
    let closed = fs::read_to_string(dir.join(&names[1])).expect("read closed");
    assert!(closed.starts_with("{\"journal\":1,"), "{closed}");
    assert!(closed.contains("\"cause\":\"start\""), "{closed}");
    // The idle tick that ended the run closed the segment.
    assert!(
        closed
            .lines()
            .last()
            .expect("a line")
            .contains("\"idle\":true"),
        "{closed}"
    );
    let current = fs::read_to_string(dir.join("q.journal")).expect("read current");
    assert_eq!(current.lines().count(), 1, "{current}");
    assert!(current.contains("\"cause\":\"rotate\""), "{current}");
}

// TODO(test-missing-fatal-tick-telemetry-wiring): a fatal tick after ticks
// that observed, with `[telemetry]`: the final export arrives before the
// exit, without the fatal tick's gauges.
#[test]
fn a_fatal_tick_is_journaled_before_the_exit() {
    // As in `permanent_forge_error_exits_one`: the mock's first message
    // fails on a closed stdout, a permanent error in the first tick.
    let (config, dir) = journaled_config("journal-fatal");
    let (reader, writer) = std::io::pipe().expect("pipe");
    drop(reader);
    let output = Command::new(env!("DORFQ_BIN"))
        .args(["run", "--config"])
        .arg(&config)
        .arg("--exit-when-idle")
        .stdout(writer)
        .output()
        .expect("spawn dorfq");
    assert_eq!(output.status.code(), Some(1), "dorfq run: {output:?}");
    let stderr = String::from_utf8(output.stderr).expect("stderr is UTF-8");
    let message = stderr
        .lines()
        .last()
        .and_then(|line| line.strip_prefix("dorfq: "))
        .expect("a dorfq: line");
    let written = fs::read_to_string(dir.join("q.journal")).expect("read journal");
    let last = written.lines().last().expect("a line");
    assert!(last.contains("\"fatal\":true"), "{written}");
    assert!(last.contains(message), "{message:?} not in {last}");
    // The tick failed observing, so its error waits for the next process's
    // first observation to give it a time; the reader releases it at the
    // journal's end.
    let events = fs::read_to_string(dir.join("q.events")).expect("read events");
    assert_eq!(events, "");
    let derived = journal(&["events"], &[dir.join("q.journal")]);
    let last = derived.lines().last().expect("an event");
    assert!(last.contains("\"kind\":\"error\""), "{derived}");
    assert!(last.contains("\"fatal\":true"), "{derived}");
}

#[test]
fn a_journal_path_holding_another_file_exits_one_and_leaves_it() {
    let (config, dir) = journaled_config("journal-foreign");
    let path = dir.join("q.journal");
    fs::write(&path, "not a journal\n").expect("write");
    let output = Command::new(env!("DORFQ_BIN"))
        .args(["run", "--config"])
        .arg(&config)
        .arg("--exit-when-idle")
        .output()
        .expect("spawn dorfq");
    assert_eq!(output.status.code(), Some(1), "dorfq run: {output:?}");
    assert!(output.stdout.is_empty(), "the forge ran: {output:?}");
    let stderr = String::from_utf8(output.stderr).expect("stderr is UTF-8");
    assert!(stderr.contains("q.journal:1:"), "stderr: {stderr:?}");
    assert_eq!(fs::read_to_string(&path).expect("read"), "not a journal\n");
}

/// Runs `dorfq journal <args>`, asserting it succeeds; returns its stdout.
fn journal(args: &[&str], segments: &[PathBuf]) -> String {
    let output = Command::new(env!("DORFQ_BIN"))
        .arg("journal")
        .args(args)
        .args(segments)
        .output()
        .expect("spawn dorfq");
    assert!(
        output.status.success(),
        "dorfq journal {args:?}: {output:?}"
    );
    String::from_utf8(output.stdout).expect("stdout is UTF-8")
}

#[test]
fn the_journal_verbs_read_a_runs_segments() {
    let (config, dir) = journaled_config("journal-verbs");
    let output = Command::new(env!("DORFQ_BIN"))
        .args(["run", "--config"])
        .arg(&config)
        .arg("--exit-when-idle")
        .output()
        .expect("spawn dorfq");
    assert!(output.status.success(), "dorfq run: {output:?}");
    let segments = segments(&dir, "q.journal");

    let events = journal(&["events"], &segments);
    assert!(events.lines().all(|l| l.starts_with("{\"t\":")), "{events}");
    // The events file beside each journal segment, concatenated, is what
    // `dorfq journal events` prints over the segments.
    let mut written = String::new();
    for segment in &segments {
        written += &beside(segment, "q.events");
    }
    assert_eq!(written, events);
    assert!(events.contains("\"kind\":\"arrive\""), "{events}");
    assert!(events.contains("\"kind\":\"land\""), "{events}");

    let table = journal(&["metrics"], &segments);
    let landed = table
        .lines()
        .find(|l| l.starts_with("landed "))
        .expect("a landed row");
    assert!(!landed.ends_with(" 0"), "{table}");
    let json = journal(&["metrics", "--json"], &segments);
    assert!(json.starts_with("{\"window_hours\":"), "{json}");
    assert_eq!(json.lines().count(), 1, "{json}");

    let report = journal(&["report", "--name", "scenario"], &segments);
    assert!(report.starts_with("{\"schema\":4,"), "{report}");
    assert!(report.contains("\"name\":\"scenario\""), "{report}");
    assert!(report.contains("\"kind\":\"all-at-once\""), "{report}");
}

/// The files in `dir` named `prefix` or `prefix.<suffix>`: the closed ones
/// in name order, then the current one.
fn segments(dir: &Path, prefix: &str) -> Vec<PathBuf> {
    let closed = format!("{prefix}.");
    let mut paths: Vec<PathBuf> = fs::read_dir(dir)
        .expect("read dir")
        .map(|e| e.expect("entry").path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with(&closed))
        })
        .collect();
    paths.sort();
    paths.push(dir.join(prefix));
    paths
}

/// The contents of the file `what` pairs with the journal segment
/// `segment`: `q.journal.<suffix>` with `q.events` gives `q.events.<suffix>`.
fn beside(segment: &Path, what: &str) -> String {
    let name = segment.file_name().and_then(|n| n.to_str()).expect("UTF-8");
    fs::read_to_string(segment.with_file_name(name.replacen("q.journal", what, 1)))
        .expect("a file beside the segment")
}

/// The `(pr, head)` of every line of `text` that `keep` accepts.
fn keys(text: &str, keep: impl Fn(&Value) -> bool) -> Vec<(u64, String)> {
    text.lines()
        .map(|line| serde_json::from_str::<Value>(line).expect("a JSON line"))
        .filter(|value| keep(value))
        .map(|value| {
            (
                value["pr"].as_u64().expect("a pr"),
                value["head"].as_str().expect("a head").to_owned(),
            )
        })
        .collect()
}

/// The `(pr, head)` of every `arrive` and `repush` event in `text`.
fn sighted(text: &str) -> Vec<(u64, String)> {
    keys(text, |event| {
        matches!(event["kind"].as_str(), Some("arrive" | "repush"))
    })
}

#[test]
fn a_journaled_run_captures_every_head_once() {
    let dir = PathBuf::from(std::env::var("TEST_TMPDIR").expect("TEST_TMPDIR under bazel test"))
        .join("capture-run");
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("create dir");
    fs::write(
        dir.join("scenario.toml"),
        "ci_ticks = 2\n\n[[pr]]\nnumber = 1\n\n[[pr]]\nnumber = 2\nrepush_at = 1\n\n\
         [[pr]]\nnumber = 3\n",
    )
    .expect("write scenario");
    let config = dir.join("dorfq.toml");
    fs::write(
        &config,
        "journal = \"q.journal\"\nevents = \"q.events\"\ncapture = \"q.capture\"\n\n\
         [forge]\nkind = \"mock\"\nscenario = \"scenario.toml\"\n\n\
         [scheduler]\nkind = \"all-at-once\"\n",
    )
    .expect("write config");
    let run = || {
        let output = Command::new(env!("DORFQ_BIN"))
            .args(["run", "--config"])
            .arg(&config)
            .arg("--exit-when-idle")
            .output()
            .expect("spawn dorfq");
        assert!(output.status.success(), "dorfq run: {output:?}");
        let stderr = String::from_utf8(output.stderr).expect("stderr is UTF-8");
        assert!(!stderr.contains("capture of"), "stderr: {stderr:?}");
    };

    run();
    let first = segments(&dir, "q.journal");
    let mut heads = sighted(&journal(&["events"], &first));
    heads.sort();
    heads.dedup();
    assert_eq!(
        heads.iter().filter(|(pr, _)| *pr == 2).count(),
        2,
        "{heads:?}"
    );
    let mut recorded = Vec::new();
    for path in segments(&dir, "q.capture") {
        let text = fs::read_to_string(&path).expect("read capture");
        for line in text.lines() {
            let record: Value = serde_json::from_str(line).expect("a JSON record");
            assert_eq!(record["capture"], 1, "{line}");
            assert_eq!(record["files"].as_array().map(Vec::len), Some(1), "{line}");
        }
        recorded.extend(keys(&text, |_| true));
    }
    recorded.sort();
    assert_eq!(recorded, heads, "each sighted head recorded exactly once");

    // The idle exit rotated all three files, so the second run starts on
    // empty ones: it records again only the heads it sees arrive, each in
    // the capture file paired with the journal segment that shows it.
    run();
    let journals = segments(&dir, "q.journal");
    assert!(journals.len() > first.len(), "{journals:?}");
    let mut all = Vec::new();
    for segment in &journals {
        let mut shown = sighted(&beside(segment, "q.events"));
        shown.sort();
        shown.dedup();
        let mut held = keys(&beside(segment, "q.capture"), |_| true);
        held.sort();
        assert_eq!(held, shown, "the capture file beside {segment:?}");
        all.extend(held);
    }
    let mut every = sighted(&journal(&["events"], &journals));
    every.sort();
    every.dedup();
    all.sort();
    all.dedup();
    assert_eq!(all, every, "deduped by key, one record per head");
}

#[test]
fn an_idle_exit_reads_every_head_still_owed() {
    // Forty pull requests arriving at once outlast the eight heads a tick
    // reads before the run goes idle; the idle exit reads the rest.
    let dir = PathBuf::from(std::env::var("TEST_TMPDIR").expect("TEST_TMPDIR under bazel test"))
        .join("capture-drain");
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("create dir");
    let mut scenario = String::from("ci_ticks = 1\n");
    for number in 1..=40 {
        scenario.push_str(&format!("\n[[pr]]\nnumber = {number}\n"));
    }
    fs::write(dir.join("scenario.toml"), scenario).expect("write scenario");
    let config = dir.join("dorfq.toml");
    fs::write(
        &config,
        "journal = \"q.journal\"\nevents = \"q.events\"\ncapture = \"q.capture\"\n\n\
         [forge]\nkind = \"mock\"\nscenario = \"scenario.toml\"\n\n\
         [scheduler]\nkind = \"all-at-once\"\n",
    )
    .expect("write config");
    let output = Command::new(env!("DORFQ_BIN"))
        .args(["run", "--config"])
        .arg(&config)
        .arg("--exit-when-idle")
        .output()
        .expect("spawn dorfq");
    assert!(output.status.success(), "dorfq run: {output:?}");
    let stderr = String::from_utf8(output.stderr).expect("stderr is UTF-8");
    assert!(!stderr.contains("capture of"), "stderr: {stderr:?}");
    let mut heads = sighted(&journal(&["events"], &segments(&dir, "q.journal")));
    heads.sort();
    heads.dedup();
    assert_eq!(heads.len(), 40, "{heads:?}");
    let mut recorded = Vec::new();
    for path in segments(&dir, "q.capture") {
        recorded.extend(keys(
            &fs::read_to_string(&path).expect("read capture"),
            |_| true,
        ));
    }
    recorded.sort();
    assert_eq!(recorded, heads, "each sighted head recorded exactly once");
}

#[test]
fn journal_events_over_a_file_that_is_not_a_journal_exits_one_naming_it() {
    let dir = PathBuf::from(std::env::var("TEST_TMPDIR").expect("TEST_TMPDIR under bazel test"))
        .join("journal-events-foreign");
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("create dir");
    let path = dir.join("notes.txt");
    fs::write(&path, "not a journal\n").expect("write");
    let output = Command::new(env!("DORFQ_BIN"))
        .args(["journal", "events"])
        .arg(&path)
        .output()
        .expect("spawn dorfq");
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    assert!(output.stdout.is_empty(), "{output:?}");
    let stderr = String::from_utf8(output.stderr).expect("stderr is UTF-8");
    assert!(
        stderr.contains(&format!("dorfq: {}:1:", path.display())),
        "stderr: {stderr:?}"
    );
}

#[test]
fn journal_report_over_an_empty_file_exits_one_saying_it_holds_no_header() {
    let dir = PathBuf::from(std::env::var("TEST_TMPDIR").expect("TEST_TMPDIR under bazel test"))
        .join("journal-report-empty");
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("create dir");
    let path = dir.join("q.journal");
    fs::write(&path, "").expect("write");
    let output = Command::new(env!("DORFQ_BIN"))
        .args(["journal", "report"])
        .arg(&path)
        .output()
        .expect("spawn dorfq");
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    assert!(output.stdout.is_empty(), "{output:?}");
    let stderr = String::from_utf8(output.stderr).expect("stderr is UTF-8");
    assert!(
        stderr.contains("dorfq: the journal holds no header"),
        "stderr: {stderr:?}"
    );
}

/// One request a [`Collector`] received.
struct Received {
    path: String,
    content_type: String,
    body: Vec<u8>,
}

/// A loopback OTLP/HTTP collector: accepts on `127.0.0.1` until stopped,
/// answers every request `200` with an empty JSON object, and records it.
struct Collector {
    port: u16,
    stop: Arc<AtomicBool>,
    received: Arc<Mutex<Vec<Received>>>,
    thread: thread::JoinHandle<()>,
}

impl Collector {
    fn start() -> Collector {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
        listener
            .set_nonblocking(true)
            .expect("nonblocking listener");
        let port = listener.local_addr().expect("local address").port();
        let stop = Arc::new(AtomicBool::new(false));
        let received = Arc::new(Mutex::new(Vec::new()));
        let thread = {
            let stop = Arc::clone(&stop);
            let received = Arc::clone(&received);
            thread::spawn(move || {
                let mut connections = Vec::new();
                while !stop.load(Ordering::SeqCst) {
                    match listener.accept() {
                        Ok((stream, _)) => {
                            let received = Arc::clone(&received);
                            connections.push(thread::spawn(move || serve(stream, &received)));
                        }
                        Err(error) if error.kind() == ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(10));
                        }
                        Err(error) => panic!("accept: {error}"),
                    }
                }
                for connection in connections {
                    connection.join().expect("a connection thread");
                }
            })
        };
        Collector {
            port,
            stop,
            received,
            thread,
        }
    }

    /// Stops accepting, waits for every connection to close, and returns
    /// what was received, in order.
    fn stop(self) -> Vec<Received> {
        self.stop.store(true, Ordering::SeqCst);
        self.thread.join().expect("the collector thread");
        std::mem::take(&mut *self.received.lock().expect("the record"))
    }
}

/// Serves requests on `stream` until the client closes it.
fn serve(mut stream: TcpStream, received: &Mutex<Vec<Received>>) {
    stream.set_nonblocking(false).expect("blocking stream");
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        let mut headers = [httparse::EMPTY_HEADER; 32];
        let mut request = httparse::Request::new(&mut headers);
        let parsed = request.parse(&buffer).expect("an HTTP request");
        if let httparse::Status::Complete(head) = parsed {
            let header = |name: &str| {
                request
                    .headers
                    .iter()
                    .find(|h| h.name.eq_ignore_ascii_case(name))
                    .map(|h| String::from_utf8(h.value.to_vec()).expect("a UTF-8 header"))
            };
            let length: usize =
                header("content-length").map_or(0, |v| v.parse().expect("a length"));
            if buffer.len() >= head + length {
                assert_eq!(request.method, Some("POST"));
                received.lock().expect("the record").push(Received {
                    path: request.path.expect("a path").to_owned(),
                    content_type: header("content-type").unwrap_or_default(),
                    body: buffer[head..head + length].to_vec(),
                });
                buffer.drain(..head + length);
                stream
                    .write_all(
                        b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 2\r\n\r\n{}",
                    )
                    .expect("answer");
                continue;
            }
        }
        match stream.read(&mut chunk).expect("read a request") {
            0 => return,
            n => buffer.extend_from_slice(&chunk[..n]),
        }
    }
}

/// Appends a `[telemetry]` table pointing at `port` to `config`.
fn with_telemetry(config: &Path, port: u16, extra: &str) {
    let mut text = fs::read_to_string(config).expect("read config");
    text.push_str(&format!(
        "\n[telemetry]\notlp_endpoint = \"http://127.0.0.1:{port}\"\ninterval_secs = 3600\n{extra}"
    ));
    fs::write(config, text).expect("write config");
}

/// The metric named `name` in an OTLP/JSON metrics request.
fn metric<'a>(body: &'a Value, name: &str) -> Option<&'a Value> {
    body["resourceMetrics"][0]["scopeMetrics"][0]["metrics"]
        .as_array()
        .expect("metrics")
        .iter()
        .find(|m| m["name"] == name)
}

/// The `asInt` values of a gauge's points.
fn ints(metric: &Value) -> Vec<u64> {
    metric["gauge"]["dataPoints"]
        .as_array()
        .expect("points")
        .iter()
        .map(|p| {
            p["asInt"]
                .as_str()
                .expect("asInt is a string")
                .parse()
                .expect("an integer")
        })
        .collect()
}

/// The string value of the attribute `key` among `attributes`.
fn attribute<'a>(attributes: &'a Value, key: &str) -> Option<&'a str> {
    attributes
        .as_array()?
        .iter()
        .find(|a| a["key"] == key)
        .and_then(|a| a["value"]["stringValue"].as_str())
}

#[test]
fn a_run_with_telemetry_pushes_its_gauges_and_every_event() {
    let collector = Collector::start();
    let (config, dir) = journaled_config("telemetry-run");
    with_telemetry(&config, collector.port, "");
    let output = Command::new(env!("DORFQ_BIN"))
        .args(["run", "--config"])
        .arg(&config)
        .arg("--exit-when-idle")
        .output()
        .expect("spawn dorfq");
    let received = collector.stop();
    assert!(output.status.success(), "dorfq run: {output:?}");
    for request in &received {
        assert!(
            request.path == "/v1/metrics" || request.path == "/v1/logs",
            "{}",
            request.path
        );
        assert_eq!(request.content_type, "application/json");
    }
    let bodies = |path: &str| -> Vec<Value> {
        received
            .iter()
            .filter(|r| r.path == path)
            .map(|r| serde_json::from_slice(&r.body).expect("a JSON body"))
            .collect()
    };
    let metrics = bodies("/v1/metrics");
    let logs = bodies("/v1/logs");
    assert!(
        !metrics.is_empty() && !logs.is_empty(),
        "{} requests",
        received.len()
    );

    let last = metrics.last().expect("a metrics body");
    let resource = &last["resourceMetrics"][0]["resource"]["attributes"];
    assert_eq!(attribute(resource, "service.name"), Some("dorfq"));
    assert_eq!(
        attribute(resource, "service.version"),
        Some(env!("CARGO_PKG_VERSION"))
    );
    let queue = metric(last, "dorfq_queue_prs").expect("dorfq_queue_prs");
    assert_eq!(ints(queue), [0, 0, 0]);
    for point in queue["gauge"]["dataPoints"].as_array().expect("points") {
        assert!(
            attribute(&point["attributes"], "status").is_some(),
            "{point}"
        );
    }
    let batches = metric(last, "dorfq_batches").expect("dorfq_batches");
    assert_eq!(ints(batches).len(), 6);

    // The idle tick that ended the run rotated the events file with the
    // journal: the closed segment, then the current one.
    let mut closed: Vec<PathBuf> = fs::read_dir(&dir)
        .expect("read dir")
        .map(|e| e.expect("entry").path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("q.events."))
        })
        .collect();
    closed.sort();
    closed.push(dir.join("q.events"));
    let events: String = closed
        .iter()
        .map(|path| fs::read_to_string(path).expect("read events"))
        .collect();
    let lines: Vec<&str> = events.lines().collect();
    let parsed: Vec<Value> = lines
        .iter()
        .map(|line| serde_json::from_str(line).expect("an event"))
        .collect();
    let landed: usize = parsed
        .iter()
        .filter(|e| e["kind"] == "land")
        .map(|e| e["members"].as_array().expect("members").len())
        .sum();
    let ejected = parsed.iter().filter(|e| e["kind"] == "eject").count();
    assert!(landed > 0 && ejected > 0, "{events}");
    let landed_prs = metric(last, "dorfq_window_landed_prs").expect("landed");
    assert_eq!(ints(landed_prs), [landed as u64]);
    let ejected_prs = metric(last, "dorfq_window_ejected_prs").expect("ejected");
    assert_eq!(ints(ejected_prs), [ejected as u64]);
    assert!(metric(last, "dorfq_tick_age_seconds").is_some());
    assert!(metric(last, "dorfq_queue_blocked_prs").is_none());
    assert!(metric(last, "dorfq_api_rate_limit_remaining").is_none());

    let records: Vec<&Value> = logs
        .iter()
        .flat_map(|body| {
            body["resourceLogs"][0]["scopeLogs"][0]["logRecords"]
                .as_array()
                .expect("log records")
        })
        .collect();
    assert_eq!(records.len(), lines.len());
    for ((record, line), event) in records.iter().zip(&lines).zip(&parsed) {
        assert_eq!(record["body"]["stringValue"], *line);
        assert_eq!(
            attribute(&record["attributes"], "kind"),
            event["kind"].as_str()
        );
    }
}

/// The process is handed the collector's address through every standard
/// OpenTelemetry endpoint variable; without `[telemetry]` it must use none.
#[test]
fn a_run_without_telemetry_opens_no_connection() {
    let collector = Collector::start();
    let endpoint = format!("http://127.0.0.1:{}", collector.port);
    let (config, _) = journaled_config("telemetry-none");
    let output = Command::new(env!("DORFQ_BIN"))
        .args(["run", "--config"])
        .arg(&config)
        .arg("--exit-when-idle")
        .env("OTEL_EXPORTER_OTLP_ENDPOINT", &endpoint)
        .env(
            "OTEL_EXPORTER_OTLP_METRICS_ENDPOINT",
            format!("{endpoint}/v1/metrics"),
        )
        .env(
            "OTEL_EXPORTER_OTLP_LOGS_ENDPOINT",
            format!("{endpoint}/v1/logs"),
        )
        .output()
        .expect("spawn dorfq");
    let received = collector.stop();
    assert!(output.status.success(), "dorfq run: {output:?}");
    assert!(received.is_empty(), "{} requests", received.len());
}

#[test]
fn telemetry_with_an_unset_authorization_variable_exits_one_naming_it() {
    let var = "DORFQ_TEST_OTLP_AUTHORIZATION_UNSET";
    let (config, _) = journaled_config("telemetry-unset-authorization");
    with_telemetry(&config, 1, &format!("authorization_env = \"{var}\"\n"));
    run_exits_one_naming(
        &config,
        &format!("dorfq: telemetry.authorization_env {var} is unset"),
        |command| {
            command.env_remove(var);
        },
    );
}
