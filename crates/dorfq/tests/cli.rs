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
/// answers every request `200` with an empty response in the request's
/// encoding — `{}` for JSON, no bytes for protobuf — and records it.
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
                let content_type = header("content-type").unwrap_or_default();
                let answer = if content_type == "application/json" {
                    "{}"
                } else {
                    ""
                };
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\n\r\n{answer}",
                    answer.len()
                );
                received.lock().expect("the record").push(Received {
                    path: request.path.expect("a path").to_owned(),
                    content_type,
                    body: buffer[head..head + length].to_vec(),
                });
                buffer.drain(..head + length);
                stream.write_all(response.as_bytes()).expect("answer");
                continue;
            }
        }
        match stream.read(&mut chunk).expect("read a request") {
            0 => return,
            n => buffer.extend_from_slice(&chunk[..n]),
        }
    }
}

/// Appends a `[telemetry]` table posting metrics to `metrics_url` and logs
/// to `logs_url` to `config`.
fn with_telemetry(config: &Path, metrics_url: &str, logs_url: &str, extra: &str) {
    let mut text = fs::read_to_string(config).expect("read config");
    text.push_str(&format!(
        "\n[telemetry]\nmetrics_endpoint = {metrics_url:?}\nlogs_endpoint = {logs_url:?}\n\
         interval_secs = 3600\n{extra}"
    ));
    fs::write(config, text).expect("write config");
}

/// A protobuf field's payload, by wire type.
#[derive(Debug)]
enum Payload {
    /// Wire type 0.
    Varint(u64),
    /// Wire type 1.
    Fixed64(u64),
    /// Wire type 2.
    Len(Vec<u8>),
}

/// One field of a protobuf message.
struct Field {
    number: u32,
    payload: Payload,
}

/// Every field of one protobuf message, in wire order; panics on a
/// malformed buffer and on wire types 3, 4 and 5, which OTLP as dorfq
/// writes it never uses. Knows nothing of OTLP: the decoders below carry
/// the field numbers, from the `.proto`.
fn fields(bytes: &[u8]) -> Vec<Field> {
    let mut at = 0;
    let varint = |at: &mut usize| {
        let mut v = 0u64;
        for shift in (0..64).step_by(7) {
            let byte = *bytes.get(*at).expect("a varint ends before the buffer");
            *at += 1;
            v |= u64::from(byte & 0x7f) << shift;
            if byte & 0x80 == 0 {
                return v;
            }
        }
        panic!("a varint longer than ten bytes");
    };
    let take = |at: &mut usize, len: usize| {
        let end = at.checked_add(len).filter(|end| *end <= bytes.len());
        let taken = &bytes[*at..end.expect("a payload ends before the buffer")];
        *at += len;
        taken
    };
    let mut fields = Vec::new();
    while at < bytes.len() {
        let key = varint(&mut at);
        let number = u32::try_from(key >> 3).expect("a field number fits 32 bits");
        let payload = match key & 7 {
            0 => Payload::Varint(varint(&mut at)),
            1 => Payload::Fixed64(u64::from_le_bytes(
                take(&mut at, 8).try_into().expect("eight bytes"),
            )),
            2 => {
                let len = usize::try_from(varint(&mut at)).expect("a length fits");
                Payload::Len(take(&mut at, len).to_vec())
            }
            other => panic!("field {number}: unhandled wire type {other}"),
        };
        fields.push(Field { number, payload });
    }
    fields
}

/// Every field numbered `number` of `message`, in wire order.
fn all(message: &[Field], number: u32) -> Vec<&Payload> {
    message
        .iter()
        .filter(|field| field.number == number)
        .map(|field| &field.payload)
        .collect()
}

/// The one field numbered `number` of `message`; panics on none or several.
fn one(message: &[Field], number: u32) -> &Payload {
    let all = all(message, number);
    assert_eq!(all.len(), 1, "exactly one field {number}");
    all[0]
}

impl Payload {
    fn bytes(&self) -> &[u8] {
        match self {
            Payload::Len(bytes) => bytes,
            other => panic!("expected a length-delimited payload, got {other:?}"),
        }
    }

    /// A length-delimited payload read as an embedded message.
    fn message(&self) -> Vec<Field> {
        fields(self.bytes())
    }

    /// A length-delimited payload read as UTF-8.
    fn string(&self) -> &str {
        std::str::from_utf8(self.bytes()).expect("a string is UTF-8")
    }

    /// An eight-byte payload read as an `sfixed64`.
    fn sfixed64(&self) -> i64 {
        match self {
            Payload::Fixed64(v) => *v as i64,
            other => panic!("expected eight bytes, got {other:?}"),
        }
    }

    /// A varint payload read as an `int64`.
    fn int64(&self) -> i64 {
        match self {
            Payload::Varint(v) => *v as i64,
            other => panic!("expected a varint, got {other:?}"),
        }
    }
}

/// An attribute's value, in either encoding.
#[derive(Debug)]
enum AttrView {
    Str(String),
    Int(i64),
}

/// A metrics request, in either encoding.
struct MetricsView {
    resource: Vec<(String, AttrView)>,
    metrics: Vec<MetricView>,
}

/// One metric of a [`MetricsView`], gauge or sum.
struct MetricView {
    name: String,
    points: Vec<PointView>,
}

/// One data point of a [`MetricView`]; `as_int` is absent for a double.
struct PointView {
    attributes: Vec<(String, AttrView)>,
    as_int: Option<i64>,
}

/// One log record of a logs request, in either encoding.
struct RecordView {
    body: String,
    attributes: Vec<(String, AttrView)>,
}

const JSON: &str = "application/json";
const PROTOBUF: &str = "application/x-protobuf";

/// The string value of the attribute `key` among `attributes`.
fn str_attribute<'a>(attributes: &'a [(String, AttrView)], key: &str) -> Option<&'a str> {
    attributes
        .iter()
        .find(|(k, _)| k == key)
        .map(|(_, v)| match v {
            AttrView::Str(s) => s.as_str(),
            AttrView::Int(i) => panic!("{key} is the integer {i}"),
        })
}

/// The integer value of the attribute `key` among `attributes`.
fn int_attribute(attributes: &[(String, AttrView)], key: &str) -> Option<i64> {
    attributes
        .iter()
        .find(|(k, _)| k == key)
        .map(|(_, v)| match v {
            AttrView::Int(i) => *i,
            AttrView::Str(s) => panic!("{key} is the string {s:?}"),
        })
}

impl MetricsView {
    /// The metric named `name`.
    fn metric(&self, name: &str) -> Option<&MetricView> {
        self.metrics.iter().find(|m| m.name == name)
    }
}

impl MetricView {
    /// The `as_int` values of the points, each required.
    fn ints(&self) -> Vec<i64> {
        self.points
            .iter()
            .map(|p| p.as_int.expect("an integer point"))
            .collect()
    }
}

/// OTLP/JSON `attributes`, absent read as none.
fn json_attributes(attributes: &Value) -> Vec<(String, AttrView)> {
    let Some(attributes) = attributes.as_array() else {
        return Vec::new();
    };
    attributes
        .iter()
        .map(|a| {
            let key = a["key"].as_str().expect("a key").to_owned();
            let value = &a["value"];
            let value = match (value["stringValue"].as_str(), value["intValue"].as_str()) {
                (Some(s), None) => AttrView::Str(s.to_owned()),
                (None, Some(i)) => AttrView::Int(i.parse().expect("an integer")),
                _ => panic!("an unexpected value: {value}"),
            };
            (key, value)
        })
        .collect()
}

/// Protobuf `KeyValue`s.
fn protobuf_attributes(key_values: Vec<&Payload>) -> Vec<(String, AttrView)> {
    key_values
        .into_iter()
        .map(|key_value| {
            let key_value = key_value.message();
            // KeyValue: key = 1, value = 2.
            let key = one(&key_value, 1).string().to_owned();
            let any = one(&key_value, 2).message();
            // AnyValue: string_value = 1, int_value = 3.
            let value = match any.as_slice() {
                [Field { number: 1, payload }] => AttrView::Str(payload.string().to_owned()),
                [Field { number: 3, payload }] => AttrView::Int(payload.int64()),
                _ => panic!("{key}: an unexpected AnyValue"),
            };
            (key, value)
        })
        .collect()
}

/// A metrics request read by its content type.
fn metrics_view(received: &Received) -> MetricsView {
    match received.content_type.as_str() {
        JSON => {
            let body: Value = serde_json::from_slice(&received.body).expect("a JSON body");
            let resource_metrics = &body["resourceMetrics"][0];
            let metrics = resource_metrics["scopeMetrics"][0]["metrics"]
                .as_array()
                .expect("metrics")
                .iter()
                .map(|m| {
                    let data = if m["gauge"].is_object() {
                        &m["gauge"]
                    } else {
                        &m["sum"]
                    };
                    MetricView {
                        name: m["name"].as_str().expect("a name").to_owned(),
                        points: data["dataPoints"]
                            .as_array()
                            .expect("points")
                            .iter()
                            .map(|p| PointView {
                                attributes: json_attributes(&p["attributes"]),
                                as_int: p["asInt"].as_str().map(|i| i.parse().expect("an integer")),
                            })
                            .collect(),
                    }
                })
                .collect();
            MetricsView {
                resource: json_attributes(&resource_metrics["resource"]["attributes"]),
                metrics,
            }
        }
        PROTOBUF => {
            let request = fields(&received.body);
            // ExportMetricsServiceRequest: resource_metrics = 1.
            let resource_metrics = one(&request, 1).message();
            // ResourceMetrics: resource = 1, scope_metrics = 2.
            let resource = one(&resource_metrics, 1).message();
            let scope_metrics = one(&resource_metrics, 2).message();
            // ScopeMetrics: metrics = 2.
            let metrics = all(&scope_metrics, 2)
                .into_iter()
                .map(|metric| {
                    let metric = metric.message();
                    // Metric: name = 1, gauge = 5, sum = 7.
                    let data = [all(&metric, 5), all(&metric, 7)].concat();
                    assert_eq!(data.len(), 1, "one of gauge or sum");
                    // Gauge, Sum: data_points = 1.
                    let points = all(&data[0].message(), 1)
                        .into_iter()
                        .map(|point| {
                            let point = point.message();
                            // NumberDataPoint: as_int = 6, attributes = 7.
                            PointView {
                                attributes: protobuf_attributes(all(&point, 7)),
                                as_int: all(&point, 6).first().map(|p| p.sfixed64()),
                            }
                        })
                        .collect();
                    MetricView {
                        name: one(&metric, 1).string().to_owned(),
                        points,
                    }
                })
                .collect();
            MetricsView {
                // Resource: attributes = 1.
                resource: protobuf_attributes(all(&resource, 1)),
                metrics,
            }
        }
        other => panic!("an unexpected content type {other:?}"),
    }
}

/// A logs request's records, read by its content type.
fn logs_view(received: &Received) -> Vec<RecordView> {
    match received.content_type.as_str() {
        JSON => {
            let body: Value = serde_json::from_slice(&received.body).expect("a JSON body");
            body["resourceLogs"][0]["scopeLogs"][0]["logRecords"]
                .as_array()
                .expect("log records")
                .iter()
                .map(|r| RecordView {
                    body: r["body"]["stringValue"]
                        .as_str()
                        .expect("a string body")
                        .to_owned(),
                    attributes: json_attributes(&r["attributes"]),
                })
                .collect()
        }
        PROTOBUF => {
            let request = fields(&received.body);
            // ExportLogsServiceRequest: resource_logs = 1.
            let resource_logs = one(&request, 1).message();
            // ResourceLogs: scope_logs = 2.
            let scope_logs = one(&resource_logs, 2).message();
            // ScopeLogs: log_records = 2.
            all(&scope_logs, 2)
                .into_iter()
                .map(|record| {
                    let record = record.message();
                    // LogRecord: body = 5, attributes = 6; AnyValue:
                    // string_value = 1.
                    let body = one(&record, 5).message();
                    RecordView {
                        body: one(&body, 1).string().to_owned(),
                        attributes: protobuf_attributes(all(&record, 6)),
                    }
                })
                .collect()
        }
        other => panic!("an unexpected content type {other:?}"),
    }
}

/// Runs `dorfq run --exit-when-idle` against a metrics collector and a
/// logs collector on their own ports and paths, with `extra` in its
/// `[telemetry]` table, and checks that every request is posted to its
/// signal's URL as `content_type` and that they carry the queue's gauges
/// and every event.
fn assert_a_run_pushes_its_gauges_and_every_event(name: &str, extra: &str, content_type: &str) {
    const METRICS_PATH: &str = "/opentelemetry/v1/metrics";
    const LOGS_PATH: &str = "/insert/opentelemetry/v1/logs";
    let metrics_collector = Collector::start();
    let logs_collector = Collector::start();
    let (config, dir) = journaled_config(name);
    with_telemetry(
        &config,
        &format!("http://127.0.0.1:{}{METRICS_PATH}", metrics_collector.port),
        &format!("http://127.0.0.1:{}{LOGS_PATH}", logs_collector.port),
        extra,
    );
    let output = Command::new(env!("DORFQ_BIN"))
        .args(["run", "--config"])
        .arg(&config)
        .arg("--exit-when-idle")
        .output()
        .expect("spawn dorfq");
    let metrics_received = metrics_collector.stop();
    let logs_received = logs_collector.stop();
    assert!(output.status.success(), "dorfq run: {output:?}");
    for (received, path) in [
        (&metrics_received, METRICS_PATH),
        (&logs_received, LOGS_PATH),
    ] {
        for request in received {
            assert_eq!(request.path, path);
            assert_eq!(request.content_type, content_type);
        }
    }
    let metrics: Vec<MetricsView> = metrics_received.iter().map(metrics_view).collect();
    let records: Vec<RecordView> = logs_received.iter().flat_map(logs_view).collect();
    assert!(
        !metrics.is_empty() && !records.is_empty(),
        "{} metrics and {} logs requests",
        metrics_received.len(),
        logs_received.len()
    );

    let last = metrics.last().expect("a metrics body");
    assert_eq!(str_attribute(&last.resource, "service.name"), Some("dorfq"));
    assert_eq!(
        str_attribute(&last.resource, "service.version"),
        Some(env!("CARGO_PKG_VERSION"))
    );
    let queue = last.metric("dorfq_queue_prs").expect("dorfq_queue_prs");
    assert_eq!(queue.ints(), [0, 0, 0]);
    for point in &queue.points {
        assert!(
            str_attribute(&point.attributes, "status").is_some(),
            "{:?}",
            point.attributes
        );
    }
    let batches = last.metric("dorfq_batches").expect("dorfq_batches");
    assert_eq!(batches.ints().len(), 6);

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
    let landed_prs = last.metric("dorfq_window_landed_prs").expect("landed");
    assert_eq!(landed_prs.ints(), [landed as i64]);
    assert_eq!(
        int_attribute(&landed_prs.points[0].attributes, "window_seconds"),
        Some(86_400)
    );
    let ejected_prs = last.metric("dorfq_window_ejected_prs").expect("ejected");
    assert_eq!(ejected_prs.ints(), [ejected as i64]);
    assert!(last.metric("dorfq_tick_age_seconds").is_some());
    assert!(last.metric("dorfq_queue_blocked_prs").is_none());
    assert!(last.metric("dorfq_api_rate_limit_remaining").is_none());

    assert_eq!(records.len(), lines.len());
    for ((record, line), event) in records.iter().zip(&lines).zip(&parsed) {
        assert_eq!(record.body, *line);
        assert_eq!(
            str_attribute(&record.attributes, "kind"),
            event["kind"].as_str()
        );
    }
}

#[test]
fn a_run_with_telemetry_pushes_its_gauges_and_every_event() {
    assert_a_run_pushes_its_gauges_and_every_event("telemetry-run", "", PROTOBUF);
}

#[test]
fn a_run_with_json_telemetry_pushes_the_same_gauges_and_events() {
    assert_a_run_pushes_its_gauges_and_every_event(
        "telemetry-run-json",
        "encoding = \"json\"\n",
        JSON,
    );
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
    with_telemetry(
        &config,
        "http://127.0.0.1:1/v1/metrics",
        "http://127.0.0.1:1/v1/logs",
        &format!("authorization_env = \"{var}\"\n"),
    );
    run_exits_one_naming(
        &config,
        &format!("dorfq: telemetry.authorization_env {var} is unset"),
        |command| {
            command.env_remove(var);
        },
    );
}
