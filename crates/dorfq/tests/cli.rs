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
