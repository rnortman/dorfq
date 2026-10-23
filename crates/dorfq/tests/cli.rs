//! Runs the built `dorfq` binary. Bazel supplies its path through
//! `DORFQ_BIN` and the shipped example configuration's through
//! `DORFQ_EXAMPLE_CONFIG`; this test does not build under `cargo test`.

use std::fs;
use std::path::PathBuf;
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
        "[forge]\nkind = \"gitlab\"\n\n[scheduler]\nkind = \"all-at-once\"\n\n\
         [driver]\ntick_millis = 0\n",
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
            "[forge]\nkind = \"mock\"\nscenario = {:?}\n\n[scheduler]\nkind = \"all-at-once\"\n\n\
             [driver]\ntick_millis = 0\n",
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

/// Runs `dorfq run` on a `github` configuration whose token is read from
/// `var`, with an `api_base` nothing listens on, and expects exit 1, no
/// stdout, and `var` named on stderr.
fn exits_one_naming_token_variable(var: &str, file: &str, set_token: impl FnOnce(&mut Command)) {
    let dir = PathBuf::from(std::env::var("TEST_TMPDIR").expect("TEST_TMPDIR under bazel test"));
    let config = dir.join(file);
    fs::write(
        &config,
        format!(
            "[forge]\nkind = \"github\"\napi_base = \"https://127.0.0.1:1\"\n\
             owner = \"alice\"\nrepo = \"widgets\"\ntrunk = \"main\"\nlabel = \"queue\"\n\
             checks = [\"check\"]\ntoken_env = \"{var}\"\n\n\
             [scheduler]\nkind = \"all-at-once\"\n\n[driver]\ntick_millis = 0\n"
        ),
    )
    .expect("write config");
    let mut command = Command::new(env!("DORFQ_BIN"));
    command.args(["run", "--config"]).arg(&config);
    set_token(&mut command);
    let output = command.output().expect("spawn dorfq");
    assert_eq!(output.status.code(), Some(1), "dorfq run: {output:?}");
    assert!(output.stdout.is_empty(), "dorfq run: {output:?}");
    let stderr = String::from_utf8(output.stderr).expect("stderr is UTF-8");
    assert!(stderr.contains(var), "stderr: {stderr:?}");
}
