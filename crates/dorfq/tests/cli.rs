//! Runs the built `dorfq` binary. Bazel supplies its path through
//! `DORFQ_BIN` and the shipped example configuration's through
//! `DORFQ_EXAMPLE_CONFIG`; this test does not build under `cargo test`.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

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
/// shipped scenario with `journal = "q.journal"`; returns the configuration
/// and the directory.
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
            "journal = \"q.journal\"\n\n[forge]\nkind = \"mock\"\nscenario = {:?}\n\n[scheduler]\nkind = \"all-at-once\"\n",
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
    let journal = fs::read_to_string(dir.join("q.journal")).expect("read journal");
    let last = journal.lines().last().expect("a line");
    assert!(last.contains("\"fatal\":true"), "{journal}");
    assert!(last.contains(message), "{message:?} not in {last}");
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
    let mut closed: Vec<PathBuf> = fs::read_dir(&dir)
        .expect("read dir")
        .map(|e| e.expect("entry").path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("q.journal."))
        })
        .collect();
    closed.sort();
    let mut segments = closed;
    segments.push(dir.join("q.journal"));

    let events = journal(&["events"], &segments);
    assert!(events.lines().all(|l| l.starts_with("{\"t\":")), "{events}");
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
